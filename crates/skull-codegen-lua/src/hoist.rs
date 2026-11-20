use super::*;

pub(crate) fn static_slots_for(
    ws: &Workspace,
    interner: &MonoInterner,
    func: &MirFunc,
    inherited_statics: &[StaticCell],
    name: &str,
    ctx: &CodegenCtx,
) -> HashMap<ustr::Ustr, String> {
    match skull_mir::global_storage::static_storage_slots(
        ws,
        interner,
        inherited_statics.iter().chain(&func.statics),
    ) {
        Ok(slots) => slots.into_iter().collect(),
        Err(error) => {
            ctx.reject(name.to_string(), CompilerError::new_no_location(error));
            HashMap::default()
        }
    }
}

/// Whether every use of `cand` is a read-only consumer, so sharing one
/// module-scope instance is unobservable. Fail-closed: any unlisted use,
/// including a new `Inst` variant, disqualifies.
fn const_hoist_uses_are_read_only(func: &MirFunc, cand: Idx<Inst>) -> bool {
    for (_, inst) in func.insts.iter() {
        let allowed = match inst {
            // A field/tuple projection and an equality test observe the value
            // without retaining or mutating it.
            Inst::Field { base, .. } | Inst::TupleIndex { base, .. } => *base == cand,
            Inst::Eq(a, b) | Inst::NotEq(a, b) => *a == cand || *b == cand,
            // Storing into cdata copies by value, so it cannot alias the shared
            // instance. A table destination stores the reference and does alias.
            Inst::MakeValueStruct {
                representation,
                fields,
                ..
            }
            | Inst::MaterializeValueStruct {
                representation,
                fields,
                ..
            } => {
                !fields.iter().any(|(_, v)| *v == cand)
                    || !matches!(
                        func.rep_arena
                            .as_ref()
                            .and_then(|arena| arena.try_get(*representation))
                            .and_then(|plan| plan.value_storage()),
                        Some(
                            skull_mir::lower::ValueStorage::GcTable
                                | skull_mir::lower::ValueStorage::LeafTable
                        )
                    )
            }
            Inst::StoreField { base, value, .. } => {
                *value != cand
                    || matches!(
                        func.insts[*base],
                        Inst::Index {
                            kind: ContainerKind::NativeBuffer
                                | ContainerKind::NativeBufferInBounds
                                | ContainerKind::NativeBufferValue(_),
                            ..
                        }
                    )
            }
            _ => false,
        };
        if allowed {
            continue;
        }
        let mut reads = false;
        inst.uses_each(&mut |u| reads |= u == cand);
        if reads {
            return false;
        }
    }
    // A terminator operand leaves the function (return value, branch
    // discriminant, yield channel), so it is never a read-only use.
    !func.blocks.iter().any(|(_, blk)| match &blk.term {
        Term::Return(vs) | Term::Yield { values: vs, .. } => vs.contains(&cand),
        Term::Branch { cond, .. } => *cond == cand,
        _ => false,
    })
}

pub(crate) fn ctor_suffix(
    func: &MirFunc,
    representation: skull_mir::lower::RepId,
    form: ValueCtorForm,
) -> Option<&'static str> {
    let arena = func.rep_arena.as_ref()?;
    let plan = arena.try_get(representation)?;
    Some(match form {
        ValueCtorForm::Leaves => "_lct",
        ValueCtorForm::Declared => match plan.value_storage() {
            Some(skull_mir::lower::ValueStorage::GcTable) => "_vt",
            Some(skull_mir::lower::ValueStorage::LeafTable) => "_lct",
            Some(skull_mir::lower::ValueStorage::CData) | None => "_ct",
        },
    })
}

/// Check a leaf-form constructor against the representation it carries.
pub fn leaf_form_violation(
    plan: &skull_mir::lower::RepPlan,
    got: &[ustr::Ustr],
) -> Option<CompilerError> {
    let Some(canonical) = plan.leaf_paths() else {
        return Some(CompilerError::new_no_location(
            "leaf-form constructor carries a non-leaf representation plan",
        ));
    };
    if got == canonical {
        return None;
    }
    Some(CompilerError::new_no_location(format!(
        "leaf-form constructor does not match its canonical leaf plan: {got:?} vs {canonical:?}"
    )))
}

/// Validate every plan-tagged constructor before rendering. A missing plan is
/// A missing plan must reach the rejection sink, not be recovered from a name.
pub(crate) fn record_representation_violations(ctx: &CodegenCtx, func: &MirFunc) {
    for (_, inst) in func.insts.iter() {
        let (name, representation, form, fields) = match inst {
            Inst::MakeValueStruct {
                name,
                representation,
                form,
                fields,
            }
            | Inst::MaterializeValueStruct {
                name,
                representation,
                form,
                fields,
            } => (*name, *representation, *form, fields),
            _ => continue,
        };
        let Some(suffix) = ctor_suffix(func, representation, form) else {
            ctx.reject(
                name.to_string(),
                CompilerError::new_no_location(format!(
                    "`{name}` value constructor carries a representation plan that does not belong to this function's arena"
                )),
            );
            continue;
        };
        if matches!(form, ValueCtorForm::Leaves) {
            let Some(plan) = func
                .rep_arena
                .as_ref()
                .and_then(|arena| arena.try_get(representation))
            else {
                ctx.reject(
                    name.to_string(),
                    CompilerError::new_no_location(format!(
                        "`{name}` leaf-form constructor has no leaf paths in its supplied plan"
                    )),
                );
                continue;
            };
            let got: Vec<ustr::Ustr> = fields.iter().map(|(n, _)| *n).collect();
            if let Some(mut error) = leaf_form_violation(plan, &got) {
                error.message = format!("`{name}` {}", error.message);
                ctx.reject(name.to_string(), error);
            }
        }
        if suffix == "_lct" && matches!(form, ValueCtorForm::Declared) {
            ctx.reject(
                name.to_string(),
                CompilerError::new_no_location(format!(
                    "`{name}` declared-form constructor selects a leaf-table plan"
                )),
            );
        }
    }
}

/// Share one module-scope constructor per identical constant value.
/// Only provably read-only uses may share it. A constructor that can reach
/// a mutable place must produce a fresh value.
pub(crate) fn collect_const_hoists(
    ws: &Workspace,
    ctx: &CodegenCtx,
    func: &MirFunc,
    base: &str,
) -> (HashMap<Idx<Inst>, u32>, Vec<String>, String) {
    let mut groups: Vec<(ustr::Ustr, skull_mir::lower::RepId, ValueCtorForm, Vec<Lit>)> =
        Vec::new();
    let mut hoist: HashMap<Idx<Inst>, u32> = HashMap::default();
    // Only insts still reached by a block: the arena keeps DCE orphans, and
    // hoisting one would materialize a nobox value the verifier cannot see.
    let mut live: Option<Vec<bool>> = None;
    for (id, inst) in func.insts.iter() {
        let Inst::MakeValueStruct {
            name,
            representation,
            form,
            fields,
        } = inst
        else {
            continue;
        };
        let live = live.get_or_insert_with(|| {
            let mut live = vec![false; func.insts.len()];
            for (_, b) in func.blocks.iter() {
                for &id in &b.insts {
                    live[raw(id) as usize] = true;
                }
            }
            live
        });
        if !live[raw(id) as usize] {
            continue;
        }
        // `IntToFloat(Const)` is still a compile-time constant: hoist its literal.
        let lits: Option<Vec<Lit>> = fields
            .iter()
            .map(|(_, op)| match &func.insts[*op] {
                Inst::Const(l) => Some(l.clone()),
                Inst::IntToFloat(x) => match &func.insts[*x] {
                    Inst::Const(l) => Some(l.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let Some(lits) = lits else { continue };
        // Nothing writes into an enum cdata after construction: a payload is not
        // an assignable place, and storing one copies or replaces the whole value.
        let immutable = skull_mir::lower::EnumPlan::for_name(ws, *name)
            .is_some_and(|plan| plan.cdata().is_some());
        if !immutable && !const_hoist_uses_are_read_only(func, id) {
            continue;
        }
        let g = match groups.iter().position(|(n, rep, ctor_form, l)| {
            *n == *name && *rep == *representation && *ctor_form == *form && *l == lits
        }) {
            Some(p) => p as u32,
            None => {
                groups.push((*name, *representation, *form, lits));
                (groups.len() - 1) as u32
            }
        };
        hoist.insert(id, g);
    }
    if groups.is_empty() {
        return (hoist, Vec::new(), String::new());
    }
    let mut names = Vec::with_capacity(groups.len());
    let mut defs = String::new();
    for (g, (name, representation, form, lits)) in groups.iter().enumerate() {
        let gname = format!("__skull_kc_{base}_{g}");
        let Some(suffix) = ctor_suffix(func, *representation, *form) else {
            ctx.reject(
                name.to_string(),
                CompilerError::new_no_location(format!(
                    "`{name}` constant constructor has no valid supplied representation plan"
                )),
            );
            continue;
        };
        let _ = write!(
            defs,
            "{gname} = {}{suffix}(",
            skull_mir::mangle::type_symbol(ws, *name)
        );
        for (i, l) in lits.iter().enumerate() {
            if i > 0 {
                defs.push_str(", ");
            }
            write_lit(&mut defs, l);
        }
        defs.push_str(")\n");
        names.push(gname);
    }
    (hoist, names, defs)
}
