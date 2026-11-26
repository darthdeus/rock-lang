use super::*;

const SCALAR_ENTRY_LOOKUP: &str = "__rock_scalar_entry";

pub fn scalar_entry_lookup_arg(mf: &MirFunc, inst: &Inst) -> Option<Idx<Inst>> {
    let Inst::CallValue { callee, args } = inst else {
        return None;
    };
    let [callback, abi] = args.as_slice() else {
        return None;
    };
    (matches!(mf.insts[*callee], Inst::Global(name) if name.as_str() == SCALAR_ENTRY_LOOKUP)
        && matches!(mf.insts[*abi], Inst::Const(Lit::Str(_))))
    .then_some(*callback)
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScalarEntryPlan {
    pub key: Ustr,
    params: Vec<Option<ScalarParameter>>,
}

#[derive(Clone, Debug, PartialEq)]
struct ScalarParameter {
    representation: RepId,
    transport: ValueShape,
}

pub fn scalar_entry_symbol(body: &str) -> String {
    format!("__rock_scalar_entry_{body}")
}

impl ScalarEntryPlan {
    pub(super) fn for_type(
        ws: &Workspace,
        scx: &ScalarCtx,
        wire: &FunctionRepPlan,
        ty: &MonoTy,
    ) -> Option<Self> {
        let MonoTy::Function { params, ret } = ty.peel_nullable().0 else {
            return None;
        };
        Self::plan(ws, scx, wire, params, ret)
    }

    pub(super) fn plan(
        ws: &Workspace,
        scx: &ScalarCtx,
        wire: &FunctionRepPlan,
        params: &[MonoTy],
        ret: &MonoTy,
    ) -> Option<Self> {
        let arena = scx.rep_arena();
        if scx.target != FnBackend::Lua
            || params.len() != wire.params().len()
            || wire.params().iter().any(|id| arena.layout(*id).splats())
            || wire.result().is_some_and(|id| arena.layout(id).splats())
        {
            return None;
        }
        let mut total = params.len() + 1; // Callable environment (unused for named functions).
        let mut shapes = Vec::with_capacity(params.len());
        for ty in params {
            let slot = Self::scalar_slot(ws, arena, ty);
            if let Some(slot) = &slot {
                total += slot.transport.leaf_count().saturating_sub(1);
            }
            shapes.push(slot);
        }
        if !shapes.iter().any(Option::is_some) || total > SIGNATURE_SCALAR_LANE_BUDGET {
            return None;
        }
        let mut key = format!("args-v1:{}:", ret.canonical());
        for (ty, shape) in params.iter().zip(&shapes) {
            use std::fmt::Write as _;
            let _ = write!(key, "{}:", ty.canonical());
            if let Some(slot) = shape {
                let paths = value_leaf_paths(&slot.transport);
                let presence = value_leaf_presence_flags(&slot.transport);
                let _ = write!(key, "{paths:?}:{presence:?}");
            }
            key.push(';');
        }
        Some(Self {
            key: crate::names::ustr(&key),
            params: shapes,
        })
    }

    fn scalar_slot(ws: &Workspace, arena: &RepArena, ty: &MonoTy) -> Option<ScalarParameter> {
        let name = ty.nominal()?;
        let sid = ws.nominals.struct_id(name)?;
        let decl = &ws.module(sid.module).structs[sid.def];
        if !decl.is_value || decl.is_nobox {
            return None;
        }
        let representation = arena
            .plan(
                ws,
                FnBackend::Lua,
                ty,
                RepUse::Value,
                &RepUseFacts::default(),
                RepGuarantees {
                    nobox: false,
                    allow_opaque: true,
                },
            )
            .ok()?;
        let transport = transport_shape(
            ws,
            arena,
            representation,
            arena.value_shape(representation)?,
        )?;
        (1..=12)
            .contains(&transport.leaf_count())
            .then_some(ScalarParameter {
                representation,
                transport,
            })
    }

    /// Seed the original parameter cells with the same values their boxed wire carried.
    /// Cleanup can then pass the leaves straight to projections and retain ordinary
    /// value storage only at uses that actually need it.
    pub fn variant(&self, ws: &Workspace, source: &MirFunc) -> Option<MirFunc> {
        let arena = source.rep_arena.as_ref()?;
        let (env, args) = match source.params.len().checked_sub(self.params.len())? {
            prefix @ 0..=1 => source.params.split_at(prefix),
            _ => return None,
        };
        let mut variant = source.clone();
        variant.scalar_entry = None;
        variant.abi_plan = None;
        variant.abi_representation = None;
        variant.nested_fns.clear();
        variant.params.clear();
        let env = env
            .first()
            .copied()
            .unwrap_or_else(|| variant.new_cell(None));
        variant.params.push(env);
        let prologue = variant.new_block();
        for (&cell, slot) in args.iter().zip(&self.params) {
            let Some(slot) = slot else {
                variant.params.push(cell);
                continue;
            };
            let shape = &slot.transport;
            let leaves = (0..shape.leaf_count())
                .map(|_| {
                    let arg = variant.new_cell(None);
                    variant.params.push(arg);
                    variant.emit(prologue, Inst::Load(arg))
                })
                .collect::<Vec<_>>();
            let value = seed_value(
                ws,
                arena,
                &mut variant,
                prologue,
                slot.representation,
                shape,
                &leaves,
            )?;
            variant.emit(prologue, Inst::Store { cell, value });
        }
        variant.set_term(prologue, Term::Goto(source.entry));
        variant.entry = prologue;
        Some(variant)
    }
}

fn transport_shape(
    ws: &Workspace,
    arena: &RepArena,
    id: RepId,
    shape: &ValueShape,
) -> Option<ValueShape> {
    if shape.nullable || ws.nominals.struct_id(shape.name).is_none() {
        return None;
    }
    let storage = arena.get(id).value_storage()?;
    let inline = if storage == ValueStorage::GcTable {
        shape::gc_table_inline_fields(ws, FnBackend::Lua, shape.name).ok()?
    } else {
        None
    };
    let mut transport = shape.clone();
    for (i, (_, field)) in transport.fields.iter_mut().enumerate() {
        let FieldShape::Nested(child) = field else {
            continue;
        };
        if let Some(inline) = inline.as_ref().and_then(|fields| fields[i].inline.as_ref()) {
            if inline != child {
                return None;
            }
            continue;
        }
        let child_transport = arena
            .plan_nominal_value(ws, FnBackend::Lua, child.name)
            .ok()
            .and_then(|id| transport_shape(ws, arena, id, child));
        if let Some(child_transport) = child_transport
            && (storage == ValueStorage::GcTable || child_transport == *child)
        {
            *child = child_transport;
        } else if storage == ValueStorage::GcTable {
            // The ordinary GC box already carries this field opaquely.
            *field = FieldShape::Prim;
        } else {
            return None;
        }
    }
    Some(transport)
}

fn seed_value(
    ws: &Workspace,
    arena: &RepArena,
    variant: &mut MirFunc,
    bb: Idx<Block>,
    id: RepId,
    shape: &ValueShape,
    leaves: &[Idx<Inst>],
) -> Option<Idx<Inst>> {
    let storage = arena.get(id).value_storage()?;
    let direct_leaves = storage == ValueStorage::LeafTable
        || (storage == ValueStorage::CData && shape.has_registry_field());
    let mut fields = Vec::new();
    if direct_leaves {
        fields.extend(
            value_leaf_paths(shape)
                .into_iter()
                .zip(leaves.iter().copied()),
        );
    } else {
        let inline = if storage == ValueStorage::GcTable {
            shape::gc_table_inline_fields(ws, FnBackend::Lua, shape.name).ok()?
        } else {
            None
        };
        let mut remaining = leaves;
        for (i, (field, layout)) in shape.fields.iter().enumerate() {
            let (part, rest) = remaining.split_at(layout.leaf_count());
            remaining = rest;
            if let Some(inline) = inline.as_ref().and_then(|fields| fields.get(i))
                && inline.is_inline()
            {
                for ((key, presence), &leaf) in inline.keys().into_iter().zip(part) {
                    let value = if presence {
                        let yes = variant.emit(bb, Inst::Const(Lit::Bool(true)));
                        variant.emit(
                            bb,
                            Inst::MakeNullableValue {
                                present: leaf,
                                value: yes,
                            },
                        )
                    } else {
                        leaf
                    };
                    fields.push((key, value));
                }
                continue;
            }
            let value = match layout {
                FieldShape::Prim => part[0],
                FieldShape::Nested(child) => {
                    let id = arena
                        .plan_nominal_value(ws, FnBackend::Lua, child.name)
                        .ok()?;
                    seed_value(ws, arena, variant, bb, id, child, part)?
                }
            };
            fields.push((*field, value));
        }
    }
    Some(variant.emit(
        bb,
        Inst::MakeValueStruct {
            name: shape.name,
            representation: id,
            form: if direct_leaves {
                ValueCtorForm::Leaves
            } else {
                ValueCtorForm::Declared
            },
            fields,
        },
    ))
}

fn is_scalar_group(func: &MirFunc, value: &ValueRepr, shape: &ValueShape) -> bool {
    match value {
        ValueRepr::Scalar(value) if shape.nullable => {
            matches!(func.insts[*value], Inst::Const(Lit::Nil))
        }
        ValueRepr::Group {
            parts,
            present,
            name,
        } if *name == shape.name && (present.is_none() || shape.nullable) => {
            parts.len() == shape.fields.len()
                && parts
                    .iter()
                    .zip(&shape.fields)
                    .all(|(part, (_, field))| match field {
                        FieldShape::Prim => true,
                        FieldShape::Nested(child) => is_scalar_group(func, part, child),
                    })
        }
        _ => false,
    }
}

enum Argument {
    Wire(Vec<Idx<Inst>>),
    Pending(ValueBundle, Vec<Idx<Inst>>),
}

impl Builder<'_> {
    fn prepare_scalar_argument(
        &mut self,
        value: ValueRepr,
        shape: &ValueShape,
    ) -> Result<ValueRepr, CompilerError> {
        let ValueRepr::Group {
            name,
            parts,
            present,
        } = value
        else {
            return Ok(value);
        };
        let parts = parts
            .into_iter()
            .zip(&shape.fields)
            .map(|(part, (_, field))| match field {
                FieldShape::Prim => self.pack_maybe_absent(part).map(ValueRepr::Scalar),
                FieldShape::Nested(child) => self.prepare_scalar_argument(part, child),
            })
            .collect::<Result<_, _>>()?;
        Ok(ValueRepr::Group {
            name,
            parts,
            present,
        })
    }

    pub(super) fn scalar_callback_call(
        &mut self,
        plan: &FunctionRepPlan,
        fn_ty: &MonoTy,
        callee: Idx<Inst>,
        args: &[Idx<Expr>],
    ) -> Result<Option<Idx<Inst>>, CompilerError> {
        // Leave literal callees eligible for the existing lambda inliner.
        if args.len() != plan.params().len() || matches!(self.f.insts[callee], Inst::Lambda { .. })
        {
            return Ok(None);
        }
        let Some(entry) = ScalarEntryPlan::for_type(self.ws, self.scx, plan, fn_ty) else {
            return Ok(None);
        };
        let MonoTy::Function { params, .. } = fn_ty.peel_nullable().0 else {
            unreachable!("scalar callback plan requires a function type");
        };
        let mut prepared = Vec::with_capacity(args.len());
        let mut can_dispatch = true;
        for (i, (&arg, shape)) in args.iter().zip(&entry.params).enumerate() {
            if let Some(slot) = shape
                && let Some(snapshot) = self.with_value_lending(true, |s| {
                    if s.lend_opaque_arg(arg).is_some() {
                        return Ok::<_, CompilerError>(None);
                    }
                    let bundle = s.lower_expr_repr(arg)?;
                    let snapshot = s.owned_value_children(bundle.value().clone())?;
                    Ok(Some(ValueBundle::new(bundle.representation(), snapshot)))
                })?
            {
                let shape = &slot.transport;
                if is_scalar_group(&self.f, snapshot.value(), shape) {
                    let id = snapshot.representation();
                    let value = self.prepare_scalar_argument(snapshot.into_value(), shape)?;
                    let leaves = self.repr_to_leaves_shape(&value, shape)?;
                    prepared.push(Argument::Pending(ValueBundle::new(id, value), leaves));
                    continue;
                }
                let value = self.pack_bundle(&snapshot)?;
                prepared.push(Argument::Wire(vec![value]));
                can_dispatch = false;
                continue;
            }

            let mut values = self.arg_values_for_slot(self.rep_slot(plan.params()[i]), arg)?;
            if let [value] = values.as_mut_slice() {
                *value = self.optional_unit_edge(arg, *value, params.get(i))?;
            }
            can_dispatch &= shape.is_none();
            prepared.push(Argument::Wire(values));
        }

        let emit_boxed = |s: &mut Self| -> Result<_, CompilerError> {
            let mut args = Vec::new();
            for arg in &prepared {
                match arg {
                    Argument::Wire(values) => args.extend(values),
                    Argument::Pending(snapshot, _) => args.push(s.pack_bundle(snapshot)?),
                }
            }
            let call = s.emit_cur(Inst::CallValue { callee, args });
            s.f.call_rep_plans.insert(call, plan.clone());
            Ok(call)
        };
        if !can_dispatch {
            return emit_boxed(self).map(Some);
        }

        let lookup = self.emit_cur(Inst::Global(crate::names::ustr(SCALAR_ENTRY_LOOKUP)));
        let tag = self.emit_cur(Inst::Const(Lit::Str(entry.key)));
        let fast = self.emit_cur(Inst::CallValue {
            callee: lookup,
            args: vec![callee, tag],
        });
        let slow_bb = self.f.new_block();
        let fast_bb = self.f.new_block();
        let join = self.f.new_block();
        let result = self.f.new_cell(None);
        self.branch_on_nil(fast, slow_bb, fast_bb);

        self.cur = fast_bb;
        let args = std::iter::once(callee)
            .chain(prepared.iter().flat_map(|arg| match arg {
                Argument::Wire(values) | Argument::Pending(_, values) => values.iter().copied(),
            }))
            .collect();
        let value = self.emit_cur(Inst::CallValue { callee: fast, args });
        self.store_cell(result, value);
        self.goto_if_open(join);

        self.cur = slow_bb;
        let value = emit_boxed(self)?;
        self.store_cell(result, value);
        self.goto_if_open(join);

        self.cur = join;
        Ok(Some(self.emit_cur(Inst::Load(result))))
    }
}
