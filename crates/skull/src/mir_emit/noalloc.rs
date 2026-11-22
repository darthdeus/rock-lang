use super::*;
use bone_semantic::hir::{FnBackend, Function};
use skull_codegen_lua::{EmittedAllocationKind as Kind, LuaEmission};
use skull_mir::mir::{Block, Inst, Lit, MirFunc, Term, VALUE_INTEGER_CHECK};

pub fn marked(f: &Function) -> bool {
    f.annotations.iter().any(|a| a.is_builtin("noalloc"))
}

pub fn required(ws: &Workspace) -> bool {
    ws.source_modules().any(|module| {
        module
            .functions
            .iter()
            .any(|(_, f)| f.backend != FnBackend::Js && marked(f))
    })
}

pub(crate) fn scalar_intrinsic(
    ws: &Workspace,
    file: &str,
    f: &Function,
    mf: &MirFunc,
    call: Idx<Inst>,
    args: &[Idx<Inst>],
) -> bool {
    let Some((FnBackend::Lua, body)) = f.inline_body else {
        return false;
    };
    if !is_stdlib_module_file(file) {
        return false;
    }
    if matches!(
        mf.call_sites.get(&call).map(|site| &site.site.op),
        Some(skull_mir::site::SiteOp::Synthesized("__nb_checked_index"))
    ) && f.name.as_str() == "__nb_checked_index"
    {
        // Only lowering's own NativeBuffer descriptor is known to have a raw len field.
        const BOUNDS_CHECK: &str = r#"if i < 0 or i >= buf.len then
error("NativeBuffer index out of bounds: index " .. tostring(i) ..
" is outside [0, " .. tostring(buf.len) .. ")")
end
return i"#;
        return body
            .as_str()
            .trim()
            .lines()
            .map(str::trim)
            .eq(BOUNDS_CHECK.lines());
    }
    let body = body.as_str().trim();
    if f.name.as_str() == "same_ref" && file == "<stdlib>/unified.rock" && body == "rawequal(a, b)"
    {
        return true;
    }
    if !f.annotations.iter().any(|a| a.is_builtin("intrinsic")) {
        return false;
    }
    match body {
        "math.abs(x)"
        | "math.sin(x)"
        | "math.cos(x)"
        | "math.tan(x)"
        | "math.sqrt(x)"
        | "math.exp(x)"
        | "math.log(x)"
        | "math.ceil(x)"
        | "math.atan2(y, x)"
        | "math.pow(a, b)"
        | "(x == 0 and x or math.floor(x))"
        | "string.len(s)"
        | "return value" => true,
        // The arena registry only stores plain maps allocated by the stdlib.
        // General Maps can have a foreign __index metamethod.
        "return map[key]" => {
            let owner = mf
                .call_sites
                .get(&call)
                .and_then(|site| mf.site_owners.get(site.owner.0 as usize));
            owner.is_some_and(|owner| {
                let module = ws.module(owner.func.module);
                module.file.as_str() == "<stdlib>/arena.rock"
                    && module.functions[owner.func.func].name.as_str() == "__arena_store_any"
            }) && args.first().is_some_and(|&map| matches!(
                mf.field_projection(map),
                Some((base, field)) if field.as_str() == "stores"
                    && matches!(mf.insts[base], Inst::Global(name) if name.as_str() == "__ARENA_REGISTRY")
            ))
        }
        _ => false,
    }
}

pub(crate) fn checked_integer_guard(mf: &MirFunc, call: Idx<Inst>) -> bool {
    skull_mir::verify::checks_enabled()
        && matches!(mf.insts[call], Inst::CallValue { callee, ref args }
            if matches!(mf.insts[callee], Inst::Global(name) if name.as_str() == VALUE_INTEGER_CHECK)
                && matches!(args.as_slice(), [_, unsigned]
                    if matches!(mf.insts[*unsigned], Inst::Const(Lit::Bool(_)))))
}

pub(super) fn validate(
    ws: &Workspace,
    f: &Function,
    mf: &MirFunc,
    emission: &LuaEmission,
) -> Result<(), CompilerError> {
    let failure = |span, reason| {
        CompilerError::new_with_label(span, format!("`@noalloc` function `{}`: {reason}", f.name))
    };
    if emission.allocation_sites.is_empty() && emission.call_sites.is_empty() {
        return Ok(());
    }

    let mut trapping = vec![false; mf.blocks.len()];
    let blocks = skull_mir::cfg::reverse_postorder(mf, mf.entry);
    // The least fixed point excludes cycles that might never reach a trap.
    loop {
        let mut changed = false;
        for &id in blocks.iter().rev() {
            let block = &mf.blocks[id];
            let traps = match block.term {
                Term::Unreachable => true,
                Term::Goto(next) => trapping[block_index(next)],
                Term::Branch {
                    then_bb, else_bb, ..
                } => trapping[block_index(then_bb)] && trapping[block_index(else_bb)],
                Term::Return(_) | Term::Yield { .. } => false,
            };
            if traps && !trapping[block_index(id)] {
                trapping[block_index(id)] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut on_trap_path = FxHashSet::default();
    for id in blocks {
        if trapping[block_index(id)] {
            on_trap_path.extend(mf.blocks[id].insts.iter().copied());
        }
    }

    let mut errors = Vec::new();
    for site in &emission.allocation_sites {
        let id = Idx::from_raw(site.mir_inst.into());
        if !on_trap_path.contains(&id) {
            errors.push(failure(
                site.span.unwrap_or(f.name_span),
                allocation_reason(mf, id, site.kind),
            ));
        }
    }
    for &id in &emission.call_sites {
        if on_trap_path.contains(&id) {
            continue;
        }
        let reason = match mf.insts[id] {
            Inst::Call { func, ref args } | Inst::CallMulti { func, ref args, .. } => {
                let module = ws.module(func.module);
                let callee = &module.functions[func.func];
                if marked(callee)
                    || scalar_intrinsic(ws, module.file.as_str(), callee, mf, id, args)
                {
                    continue;
                }
                format!(
                    "calls non-@noalloc `{}`",
                    skull_mir::mangle::declared_name(callee.name.as_str())
                )
            }
            Inst::CallValue { .. } if checked_integer_guard(mf, id) => continue,
            Inst::CallValue { .. } | Inst::CallValueMulti { .. } => {
                "calls an indirect function value".to_string()
            }
            _ => unreachable!("the Lua emitter recorded a non-call"),
        };
        errors.push(failure(
            mf.inst_spans.get(id).copied().unwrap_or(f.name_span),
            reason,
        ));
    }
    errors.sort_by_key(|error| {
        let span = error.labels[0].span;
        (span.file, span.offset_range())
    });
    let mut errors = errors.into_iter();
    match errors.next() {
        Some(first) => Err(first.with_siblings(errors)),
        None => Ok(()),
    }
}

pub(super) fn merge_diagnostics(mut first: CompilerError, other: CompilerError) -> CompilerError {
    let mut seen: FxHashSet<_> = std::iter::once(&first)
        .chain(first.siblings.iter())
        .map(|error| {
            (
                error.labels.first().map(|label| label.span),
                error.message.clone(),
            )
        })
        .collect();
    let mut other = other;
    let siblings = std::mem::take(&mut other.siblings);
    first
        .siblings
        .extend(std::iter::once(other).chain(siblings).filter(|error| {
            seen.insert((
                error.labels.first().map(|label| label.span),
                error.message.clone(),
            ))
        }));
    first
}

fn block_index(id: Idx<Block>) -> usize {
    u32::from(id.into_raw()) as usize
}

pub(super) fn reject_inline(f: &Function) -> CompilerError {
    CompilerError::new_with_label(
        f.name_span,
        format!(
            "`@noalloc` function `{}`: cannot verify a verbatim inline Lua body",
            f.name
        ),
    )
}

pub(crate) fn reject_generated(
    f: &Function,
    site: &crate::allocation_report::AllocationSite,
) -> CompilerError {
    let crate::allocation_report::AllocationKind::Emitted(kind) = site.kind else {
        unreachable!("generated allocation is not an opaque call");
    };
    CompilerError::new_with_label(
        site.span.unwrap_or(f.name_span),
        format!(
            "`@noalloc` function `{}`: {reason}",
            f.name,
            reason = allocation_description(kind)
        ),
    )
}

pub(crate) fn allocation_description(kind: Kind) -> &'static str {
    match kind {
        Kind::StringConcat => "string concatenation",
        Kind::StringConversion => "string conversion",
        Kind::StringFormat => "string formatting",
        Kind::TupleConstruction => "constructs a tuple table",
        Kind::VecConstruction | Kind::VecColumnConstruction | Kind::VecColumnHeader => {
            "constructs a Vec"
        }
        Kind::CoroutineFrame => "constructs a coroutine frame",
        Kind::FfiArray => "allocates an FFI array",
        Kind::FfiReturnValue => "boxes an FFI return value",
        Kind::FfiView => "allocates an FFI pointer view",
        Kind::ContainerGrowth => "may grow a container table",
        Kind::StructConstruction
        | Kind::ValueConstruction
        | Kind::ExplicitValueMaterialization
        | Kind::EnumConstruction
        | Kind::DynamicEnumConstruction => "constructs a table",
        Kind::ClosureEnvironment => "allocates a closure environment or shared capture",
    }
}

fn allocation_reason(mf: &MirFunc, id: Idx<Inst>, kind: Kind) -> String {
    match kind {
        Kind::StructConstruction
        | Kind::ValueConstruction
        | Kind::ExplicitValueMaterialization
        | Kind::EnumConstruction
        | Kind::DynamicEnumConstruction => {
            let (name, _) = crate::allocation_report::allocation_aggregate(&mf.insts[id]);
            name.map_or_else(
                || allocation_description(kind).to_string(),
                |name| format!("boxes `{name}`"),
            )
        }
        Kind::ClosureEnvironment => {
            let Inst::MakeEnv { fields, .. } = &mf.insts[id] else {
                unreachable!()
            };
            let names = fields
                .iter()
                .filter_map(|(_, value)| match mf.insts[*value] {
                    Inst::Load(cell) => mf.cells[cell].name.map(|name| format!("`{name}`")),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if names.is_empty() {
                allocation_description(kind).to_string()
            } else {
                format!("closure captures {}", names.join(", "))
            }
        }
        _ => allocation_description(kind).to_string(),
    }
}
