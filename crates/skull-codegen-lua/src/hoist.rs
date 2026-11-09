//! Static slots, constant hoisting and representation checks.

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

/// Whether every use of `cand` is a provably READ-ONLY consumer, so sharing one
/// module-scope instance across all of them is unobservable.
///
/// FAIL-CLOSED by construction: the allow-list is tiny and anything else --
/// including a new `Inst` variant -- disqualifies. The inverse (list the
/// escaping consumers) would silently start sharing a mutable value the day the
/// enum grows.
fn const_hoist_uses_are_read_only(func: &MirFunc, cand: Idx<Inst>) -> bool {
    for (_, inst) in func.insts.iter() {
        let allowed = match inst {
            // A field/tuple projection and an equality test observe the value
            // without retaining or mutating it.
            Inst::Field { base, .. } | Inst::TupleIndex { base, .. } => *base == cand,
            Inst::Eq(a, b) | Inst::NotEq(a, b) => *a == cand || *b == cand,
            // Storing INTO cdata copies by value, so the destination cannot alias
            // the shared instance: an FFI-clean `<Name>_ct(..)` ctor copies each
            // field, and a `slot.f = v` into a NativeBuffer element writes bytes.
            // A TABLE destination (plain struct, GC-tier `<Name>_vt`, Vec element,
            // global, cell) stores the REFERENCE and is what aliases.
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
                            kind: ContainerKind::NativeBuffer | ContainerKind::NativeBufferValue(_),
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
        regalloc::inst_uses(inst, &mut |u| reads |= u == cand);
        if reads {
            return false;
        }
    }
    // A terminator operand leaves the function (return value, branch
    // discriminant, yield channel), so it is never a read-only use.
    !func.blocks.iter().any(|(_, blk)| match &blk.term {
        Term::Return(vs) | Term::Yield { values: vs, .. } => vs.contains(&cand),
        Term::Branch { cond, .. } => *cond == cand,
        Term::Switch { discr, .. } => *discr == cand,
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
/// Callers supply the plan; this routine never reconstructs one from a name or
/// from the punctuation of the operand keys.
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
/// an internal compiler error; it must reach the ordinary rejection sink rather
/// than making emission recover a tier from a name or operand spelling.
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
            if let Some(mut error) = leaf_form_violation(&plan, &got) {
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

/// Group every all-constant `MakeValueStruct` (every field operand a `Const`) by
/// (struct name, field literals). Returns `(inst -> group, group -> global name,
/// def lines)`: each group becomes one function-owned global
/// `__skull_kc_<fn>_<g> = <Name>_ct(<lits>)`. A module-scope global is built once
/// at chunk load, so it is never worse than a per-use ctor (which it equals on a
/// trace via SINK and beats in the interpreter), unlike a per-CALL function-entry
/// local which defeats SINK.
///
/// Hoisting is NOT unconditional: it is sound only while the shared instance
/// cannot be mutated. The place model legalized leaf writes through mutable
/// storage, so a constant that reaches a mutable
/// place must be materialized FRESH per construction -- otherwise `a.p.x = 5`
/// through one holder rewrites every other holder of that literal, including an
/// unrelated global (`tests/values/const_hoist.rock`). Candidates whose
/// uses are not provably read-only emit their ctor inline, exactly as before the
/// hoist existed.
pub(crate) fn collect_const_hoists(
    ws: &Workspace,
    ctx: &CodegenCtx,
    func: &MirFunc,
    base: &str,
) -> (HashMap<Idx<Inst>, u32>, Vec<String>, String) {
    let mut groups: Vec<(ustr::Ustr, skull_mir::lower::RepId, ValueCtorForm, Vec<Lit>)> =
        Vec::new();
    let mut hoist: HashMap<Idx<Inst>, u32> = HashMap::default();
    // Only insts still REACHED BY A BLOCK. The arena keeps orphans (DCE unlinks
    // from blocks, it does not free), and hoisting one emits a module-scope ctor
    // call for a value the program never builds -- which for a nobox type is a
    // materialization the verifier cannot see, since it walks `block.insts`.
    let mut live = vec![false; func.insts.len()];
    for (_, b) in func.blocks.iter() {
        for &id in &b.insts {
            live[raw(id) as usize] = true;
        }
    }
    for (id, inst) in func.insts.iter() {
        if !live[raw(id) as usize] {
            continue;
        }
        let Inst::MakeValueStruct {
            name,
            representation,
            form,
            fields,
        } = inst
        else {
            continue;
        };
        // An int->float-subsumed constant field (`IntToFloat(Const)`) is still a
        // compile-time constant: hoist as its inner literal (identity on luajit),
        // so a `Vec2 { x: 0 }` ctor stays hoistable.
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
        if !const_hoist_uses_are_read_only(func, id) {
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
