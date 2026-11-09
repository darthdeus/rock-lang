//! Global initializers: dependency ordering, init lines and the mutable-global layout.

use super::*;

/// The globals a function reads DIRECTLY plus the (statically-resolved) functions
/// it calls, walking only blocks reachable from `mf.entry`. A lambda's body is a
/// sub-CFG unreachable from entry (its reads fire when the closure is later
/// *called*, not at this function's load time), so it is intentionally not
/// descended into — keeping init ordering from over-constraining on closures.
pub(super) fn direct_refs(ws: &Workspace, cg: &CodegenCtx, mf: &MirFunc) -> (Vec<Ustr>, Vec<Ustr>) {
    let mut globals = Vec::new();
    let mut calls = Vec::new();
    let mut seen = vec![false; mf.blocks.len()];
    let mut stack = vec![mf.entry];
    while let Some(b) = stack.pop() {
        if std::mem::replace(&mut seen[skull_mir::cfg::dense::bx(b)], true) {
            continue;
        }
        let block = &mf.blocks[b];
        for &iid in &block.insts {
            match &mf.insts[iid] {
                Inst::Global(n) => globals.push(*n),
                Inst::Call { func, .. } | Inst::CallMulti { func, .. } => {
                    calls.push(cg.mangled_name(ws, *func));
                }
                _ => {}
            }
        }
        match &block.term {
            Term::Goto(t) => stack.push(*t),
            Term::Branch {
                then_bb, else_bb, ..
            } => {
                stack.push(*then_bb);
                stack.push(*else_bb);
            }
            Term::Switch { cases, default, .. } => {
                stack.extend(cases.iter().map(|(_, t)| *t));
                stack.push(*default);
            }
            Term::Yield { resume, .. } => stack.push(*resume),
            Term::Return(_) | Term::Unreachable => {}
        }
    }
    (globals, calls)
}

/// Sort the pending init lines into load-time order (see
/// [`skull_mir::reach::order_global_inits`]).
pub(super) fn sort_global_init_lines(
    inits: Vec<PendingInit<String>>,
    leaf_owner: &FxHashMap<Ustr, Ustr>,
) -> Result<Vec<String>, CompilerError> {
    skull_mir::reach::order_global_inits(inits, leaf_owner).map_err(|stuck| {
        let names: Vec<&str> = stuck.iter().map(|name| name.as_str()).collect();
        CompilerError::new_no_location(format!(
            "skull: global initializers are mutually recursive: {}",
            names.join(", ")
        ))
    })
}

const LUA_INIT_PART_BYTES: usize = 32 * 1024;

pub(super) fn wrap_large_lua_section(name: &str, body: &str) -> String {
    if body.len() <= LUA_INIT_PART_BYTES {
        return body.to_string();
    }
    format!("local function {name}()\n{body}end\n{name}()\n")
}

pub(super) fn state_initializer_lua(init_lines: &[String]) -> String {
    let mut parts: Vec<Vec<&str>> = vec![Vec::new()];
    let mut part_bytes = 0;
    for line in init_lines {
        if part_bytes > 0 && part_bytes + line.len() > LUA_INIT_PART_BYTES {
            parts.push(Vec::new());
            part_bytes = 0;
        }
        parts.last_mut().unwrap().push(line.as_str());
        part_bytes += line.len();
    }

    let mut out = String::new();
    if parts.len() == 1 {
        out.push_str("function __rock_state_initialize()\n");
        for line in &parts[0] {
            out.push_str(line);
        }
        out.push_str("end\n__rock_state_initialize()\n");
        return out;
    }

    for (index, part) in parts.iter().enumerate() {
        writeln!(out, "local function __rock_state_initialize_part_{index}()").unwrap();
        for line in part {
            out.push_str(line);
        }
        out.push_str("end\n");
    }
    out.push_str("function __rock_state_initialize()\n");
    for index in 0..parts.len() {
        writeln!(out, "__rock_state_initialize_part_{index}()").unwrap();
    }
    out.push_str("end\n__rock_state_initialize()\n");
    out
}

/// The assignment target of a global's init line: its own name, or -- for a
/// `nobox` global, whose storage IS its leaves -- the comma-separated leaf list
/// bound from the initializer's multiple return. Leaf names are compiler-minted
/// top-level names, so they go through the collision sink.
pub(super) fn global_init_target(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
    g: &bone_semantic::hir::GlobalDef,
) -> Result<String, CompilerError> {
    let plan = skull_mir::lower::GlobalPlan::for_declared_type(ws, scx, g.ty)
        .map_err(|e| CompilerError::new_no_location(e.message))?;
    // The LHS is an identifier, so it is the global's emitted symbol; a v1
    // DefKey never reaches the artifact.
    let symbol = skull_mir::mangle::symbol_of_key(g.name);
    let Some(leaves) = plan.leaf_names(symbol) else {
        return Ok(symbol.to_string());
    };
    Ok(leaves
        .iter()
        .map(|l| cg.mint_synthetic(l.to_string()))
        .collect::<Vec<_>>()
        .join(", "))
}

/// The load-time initialization of one mutable global. It runs only when this
/// load created the global storage: a program loaded over live storage keeps
/// every value the running program wrote.
fn global_init_line(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
    g: &bone_semantic::hir::GlobalDef,
    init_name: &str,
) -> Result<String, CompilerError> {
    let plan = skull_mir::lower::GlobalPlan::for_declared_type(ws, scx, g.ty).map_err(|_| {
        CompilerError::new_no_location(format!("no storage plan for global `{}`", g.name))
    })?;
    let slot = |leaf: Ustr| {
        cg.state_slot(leaf).ok_or_else(|| {
            CompilerError::new_no_location(format!(
                "global `{}` has no compiler-owned storage slot for `{leaf}`",
                g.name
            ))
        })
    };
    let cell = cg.state_table();
    match plan.leaf_names(skull_mir::mangle::symbol_of_key(g.name)) {
        Some(leaves) => {
            let mut line = String::from("if __rock_globals_fresh then\n");
            line.push_str(&format!(
                "  local __rock_init_values = {{ {init_name}() }}\n"
            ));
            for (index, leaf) in leaves.iter().enumerate() {
                line.push_str(&format!(
                    "  {cell}[{}] = __rock_init_values[{}]\n",
                    slot(*leaf)?,
                    index + 1
                ));
            }
            line.push_str("end\n");
            Ok(line)
        }
        None => Ok(format!(
            "if __rock_globals_fresh then {cell}[{}] = {init_name}() end\n",
            slot(g.name)?
        )),
    }
}

pub fn lua_global_state_slots(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
) -> Result<std::collections::BTreeMap<ustr::Ustr, u32>, CompilerError> {
    let mut slots = skull_mir::global_storage::global_storage_keys(ws, scx)
        .map_err(CompilerError::new_no_location)?
        .into_iter()
        .collect::<Vec<_>>();
    slots.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(slots
        .into_iter()
        .enumerate()
        .map(|(index, (physical, _))| {
            let slot = u32::try_from(index + 1).expect("Lua global slot population exceeds u32");
            (physical, slot)
        })
        .collect())
}

/// The line that opens (or, on a later load, validates) the global storage and
/// seats this chunk's storage cell. The table is sized to its rehash fixpoint:
/// the array part at the highest slot's next power of two, plus one for index
/// 0, which is what LuaJIT's `rehashtab` settles on once more than half of that
/// range is populated, so a later rehash cannot move it -- and a hash part with
/// twice the statics' string keys, so no static's first store overflows it.
pub(super) fn globals_open_lua(layout: &str, cg: &CodegenCtx, static_count: usize) -> String {
    let narr = cg.state_slot_max().next_power_of_two() + 1;
    let nhash = (static_count * 4).next_power_of_two().max(16);
    format!(
        "{} = __rock_globals_open({layout:?}, {narr}, {nhash})\n",
        skull_codegen_lua::STATE_CELL
    )
}

/// The global directory every reflection reader and the `_G` bridge use.
pub(super) fn globals_directory_lua(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
) -> Result<String, CompilerError> {
    use skull_codegen_lua::lua_string;
    let entries = skull_mir::global_storage::global_directory(ws, scx, |leaf| {
        cg.state_slot(leaf).map(|slot| slot.to_string())
    })
    .map_err(CompilerError::new_no_location)?;
    let mut out = String::from("_G.__rock_globals_publish({");
    for entry in entries {
        let physical = entry
            .physical
            .iter()
            .map(|leaf| lua_string(leaf))
            .collect::<Vec<_>>()
            .join(",");
        let _ = writeln!(
            out,
            "{{name={},identity={},physical={{{physical}}},slots={{{}}}}},",
            lua_string(&entry.name),
            lua_string(&entry.identity),
            entry.slots.join(",")
        );
    }
    out.push_str("})\n");
    Ok(out)
}

/// Lower + emit one global's initializer fn, recording its `pending_init` line.
pub(super) fn emit_global_unit(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    mid: ModuleId,
    gid: Idx<bone_semantic::hir::GlobalDef>,
) -> UnitOut {
    let mut out = UnitOut::default();
    let g = &ws.module(mid).globals[gid];
    let init_name = crate::global_init_fn_name(skull_mir::mangle::symbol_of_key(g.name).as_str());
    out.attribution.owner = format!("m{}:global{}", mid.index(), u32::from(gid.into_raw()));
    let tl = Instant::now();
    let owner = skull_mir::concrete::ConcreteOwner::Global {
        module: mid,
        index: gid.into_raw().into_u32(),
    };
    let concrete_plan = concrete.plan_for_owner(&owner);
    let lowered = match concrete_plan {
        Some(plan) => skull_mir::lower::lower_global_with_plans(
            ws,
            scx,
            plan,
            &concrete.plans,
            ustr::Ustr::from(init_name.as_str()),
        ),
        None if ws
            .module(mid)
            .bodies
            .get(&bone_semantic::hir::BodyOwner::Global(gid))
            .and_then(|body| body.root)
            .is_none() =>
        {
            Ok(None)
        }
        None => Err(CompilerError::new_with_label(
            g.name_span,
            "no concrete plan for global initializer".to_string(),
        )),
    };
    out.times.lower = tl.elapsed();
    match lowered {
        Ok(Some(mut mf)) => {
            out.attribution.examined = true;
            out.attribution.hir_nodes = hir_nodes_for_owner(ws, mid, BodyOwner::Global(gid));
            out.attribution.mir_instructions = mf.insts.len();
            out.attribution.lower_facts = mf.lower_attribution.clone();
            out.times.lower_work = out.attribution.mir_instructions > 0;
            // A global-initializer body can carry lambdas (e.g. a `[TileDef]`
            // literal with fn fields) -- route them through the shared pass, same
            // as an ordinary fn. Their global reads are NOT init-ordering
            // constraints (the fn value is stored, not called at init), so `refs`
            // is computed on the post-pass `mf` where those reads live in the
            // hoisted siblings, not the main body.
            if let Err(e) = convert_unit_body(ws, cg, &mut mf, &init_name, &mut out.times) {
                out.hard_error = Some(e);
                return out;
            }
            let owner = bone_semantic::hir::BodyOwner::Global(gid);
            let plan = concrete_plan.expect("a lowered global has a concrete plan");
            out.attribution.always_lowered = true;
            out.attribution
                .always_callee_keys
                .extend(plan.callees.values().cloned());
            let ctx = skull_mir::bound::concrete_body_ctx(
                ws,
                mid,
                owner,
                &plan.bound_tree,
                scx.interner(),
            );
            let tv = Instant::now();
            let check = check_unit_spec_bindings(
                ws,
                &ctx,
                mid,
                g.name.as_str(),
                &mf,
                g.name_span,
                &mut out.specs,
            );
            out.times.validation += tv.elapsed();
            out.times.validation_work = true;
            if let Err(e) = check {
                out.hard_error = Some(e);
                return out;
            }
            let key = Ustr::from(&init_name);
            skull_codegen_lua::regalloc_time_reset();
            let te = Instant::now();
            // A decorator / global-initializer body runs once at chunk load,
            // outside the definitions section the binding pass covers.
            let emission = emit_named_with_allocation_report(
                ws,
                cg,
                scx.interner(),
                &init_name,
                &mf,
                &[],
                None,
            );
            let mut lua = emission.lua;
            if let Some(body) = crate::allocation_report::collect_emitted_body(
                ws,
                scx.interner(),
                &init_name,
                g.name.as_str(),
                &mf,
                emission.allocation_sites,
            ) {
                out.allocation_bodies.push(body);
            }
            let root_emit = te.elapsed();
            out.times.emit += root_emit;
            out.times.emit_work |= root_emit > Duration::ZERO;
            let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(root_emit);
            out.times.regalloc += regalloc;
            out.times.regalloc_work |= regalloc > Duration::ZERO;
            if let Err(e) = emit_pass_nested_fns(
                ws,
                cg,
                scx,
                &mut mf.nested_fns,
                &mf.statics,
                Some(&mut out.allocation_bodies),
                &mut lua,
                &mut out.times,
            ) {
                out.hard_error = Some(e);
                return out;
            }
            emit_lowered(
                ws,
                cg,
                &mut out,
                &mf,
                lua,
                &init_name,
                Some(key),
                Duration::ZERO,
            );
            out.attribution.times = out.times;
            // Hot-reload init line. A `const` is a compile-time constant: tweaking
            // the source literal should show the new value next reload, so its init
            // runs UNCONDITIONALLY (no preserve guard).
            //
            // A mutable `global` holds live runtime state we WANT to survive the
            // re-dofile, so it keeps the preservation guard: on a reload
            // (`_G.HOT_RELOAD`), skip the assignment when this global existed with
            // the SAME type in the previous run. Cold start (HOT_RELOAD nil) always
            // runs the init. The type string is bone's render of `g.ty`, also
            // written into `__rock_known_globals` below (which lists const + mutable
            // alike); both come from the same render so "type unchanged" compares
            // equal.
            // Same TYPE is not same LAYOUT, and this guard deliberately does not
            // detect the difference: a layout drift restarts the process upstream
            // (`shape_drift_tests::marking_a_type_nobox_requests_restart`), and a
            // respawn is a cold start that never reaches this branch.
            let target = match global_init_target(ws, scx, cg, g) {
                Ok(target) => target,
                Err(e) => {
                    out.hard_error = Some(e);
                    return out;
                }
            };
            let line = if g.is_const {
                format!("{target} = {init_name}()\n")
            } else {
                match global_init_line(ws, scx, cg, g, &init_name) {
                    Ok(line) => line,
                    Err(error) => {
                        out.hard_error = Some(error);
                        return out;
                    }
                }
            };
            out.pending_init = Some((g.name, key, line));
        }
        Ok(None) => {}
        Err(e) => out.hard_error = Some(e),
    }
    out
}

#[cfg(test)]
mod state_initializer_tests {
    use super::{state_initializer_lua, wrap_large_lua_section};

    #[test]
    fn large_global_initializer_is_split_without_reordering() {
        let lines = vec![
            format!("A = \"{}\"\n", "a".repeat(20_000)),
            format!("B = \"{}\"\n", "b".repeat(20_000)),
            format!("C = \"{}\"\n", "c".repeat(20_000)),
        ];
        let lua = state_initializer_lua(&lines);

        assert!(lua.contains("local function __rock_state_initialize_part_0()"));
        assert!(lua.contains("local function __rock_state_initialize_part_1()"));
        assert!(lua.contains("__rock_state_initialize_part_0()"));
        assert!(lua.contains("__rock_state_initialize_part_1()"));
        assert!(lua.find("A =").unwrap() < lua.find("B =").unwrap());
        assert!(lua.find("B =").unwrap() < lua.find("C =").unwrap());
        assert_eq!(lua.matches("A =").count(), 1);
        assert_eq!(lua.matches("B =").count(), 1);
        assert_eq!(lua.matches("C =").count(), 1);
    }

    #[test]
    fn large_publication_section_is_nested_without_reordering() {
        let body = "A = 1\nB = 2\n".repeat(20_000);
        let lua = wrap_large_lua_section("__rock_publication_part", &body);

        assert!(lua.starts_with("local function __rock_publication_part()\n"));
        assert!(lua.ends_with("end\n__rock_publication_part()\n"));
        assert_eq!(lua.matches("A = 1").count(), 20_000);
        assert_eq!(lua.matches("B = 2").count(), 20_000);
    }
}
