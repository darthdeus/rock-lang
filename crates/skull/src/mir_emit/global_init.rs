use super::*;

/// The globals a function reads directly and the functions it calls, from
/// blocks reachable from `mf.entry` only: a lambda body runs when the
/// closure is called, not at load time.
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
            Term::Yield { resume, .. } => stack.push(*resume),
            Term::Return(_) | Term::Unreachable => {}
        }
    }
    (globals, calls)
}

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

/// A `nobox` global's storage is its leaves, bound from the initializer's
/// multiple return; the minted leaf names go through the collision sink.
pub(super) fn global_init_target(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
    g: &bone_semantic::hir::GlobalDef,
) -> Result<String, CompilerError> {
    let plan = skull_mir::lower::GlobalPlan::for_declared_type(ws, scx, g.ty)
        .map_err(|e| CompilerError::new_no_location(e.message))?;
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

/// Runs only when this load created the global storage: a program loaded over
/// live storage keeps every value the running program wrote.
fn global_init_line(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
    g: &bone_semantic::hir::GlobalDef,
    init: &str,
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
            line.push_str(&format!("  local __rock_init_values = {{ {init} }}\n"));
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
            "if __rock_globals_fresh then {cell}[{}] = {init} end\n",
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

/// The table is sized to its rehash fixpoint: the array part at the highest
/// slot's next power of two plus one for index 0, which is where LuaJIT's
/// `rehashtab` settles, and a hash part with twice the statics' string keys.
pub(super) fn globals_open_lua(layout: &str, cg: &CodegenCtx, static_count: usize) -> String {
    let narr = cg.state_slot_max().next_power_of_two() + 1;
    let nhash = (static_count * 4).next_power_of_two().max(16);
    format!(
        "{} = __rock_globals_open({layout:?}, {narr}, {nhash})\n",
        skull_codegen_lua::STATE_CELL
    )
}

pub(super) struct GlobalsDirectoryLua {
    pub publish: String,
    pub accessors: String,
    pub ctors: Vec<Ustr>,
}

fn accessor_fields(
    entry: &skull_mir::global_storage::GlobalDirectoryEntry,
    shape: &skull_mir::lower::NoboxGlobalAccessorShape,
    ctor: &str,
) -> Result<String, CompilerError> {
    let name = entry.logical_name;
    let (presence, slots) = if shape.nullable {
        let (presence, slots) = entry.slots.split_first().ok_or_else(|| {
            CompilerError::new_no_location(format!(
                "nullable nobox global `{name}` has no presence leaf"
            ))
        })?;
        (Some(presence), slots)
    } else {
        (None, entry.slots.as_slice())
    };
    if slots.len() != shape.paths.len() {
        return Err(CompilerError::new_no_location(format!(
            "nobox global `{name}` has {} leaves but {} leaf paths",
            slots.len(),
            shape.paths.len()
        )));
    }
    if let Some(why) = &shape.unbuildable {
        return Err(CompilerError::new_no_location(format!(
            "nobox global `{name}` of type `{}` cannot be rebuilt: {why}",
            shape.ty_name
        )));
    }
    let cells: Vec<_> = slots
        .iter()
        .map(|key| format!("__rock_globals[{key}]"))
        .collect();
    let args = cells.join(", ");
    let writes = cells
        .iter()
        .zip(&shape.paths)
        .map(|(cell, path)| {
            // Table boxes store dotted paths as literal keys; cdata uses nested fields.
            if shape.storage == skull_mir::lower::ValueStorage::CData {
                format!("{cell} = v.{path}")
            } else {
                format!("{cell} = v[{:?}]", path.as_str())
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    let get = format!("return {ctor}({args})");
    let (get, set) = if let Some(key) = presence {
        let clear = cells
            .iter()
            .map(|cell| format!("{cell} = nil"))
            .collect::<Vec<_>>()
            .join("; ");
        (
            format!("if not __rock_globals[{key}] then return nil end {get}"),
            format!("__rock_globals[{key}] = v ~= nil; if v ~= nil then {writes} else {clear} end"),
        )
    } else {
        (get, writes)
    };
    Ok(format!(
        "get=function() {get} end,set=function(v) {set} end"
    ))
}

pub(super) fn globals_directory_lua(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
    with_accessors: bool,
) -> Result<GlobalsDirectoryLua, CompilerError> {
    use skull_codegen_lua::lua_string;
    let entries = skull_mir::global_storage::global_directory(ws, scx, |leaf| {
        cg.state_slot(leaf).map(|slot| slot.to_string())
    })
    .map_err(CompilerError::new_no_location)?;
    let mut out = String::from("_G.__rock_globals_publish({");
    let mut accessors = String::new();
    let mut ctors = Vec::new();
    if with_accessors {
        accessors.push_str("\n_G.__rock_global_decorate({");
    }
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
        let shape = match skull_mir::lower::nobox_global_accessor_shape(ws, scx, &entry.plan) {
            Ok(Some(shape)) => shape,
            Err(e) if with_accessors => return Err(CompilerError::new_no_location(e.message)),
            _ => continue,
        };
        let ctor = format!(
            "{}{}",
            skull_mir::mangle::type_symbol(ws, shape.ty_name),
            shape.ctor_suffix
        );
        if shape.ctor_suffix != "_ct" {
            ctors.push(Ustr::from(&ctor));
        }
        if with_accessors {
            let fields = accessor_fields(&entry, &shape, &ctor)?;
            let _ = writeln!(accessors, "[{:?}]={{{fields}}},", entry.identity);
        }
    }
    if with_accessors {
        accessors.push_str("})\n");
    }
    out.push_str("})\n");
    Ok(GlobalsDirectoryLua {
        publish: out,
        accessors,
        ctors,
    })
}

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
    let init_name = format!("__skull_ginit_{}", skull_mir::mangle::symbol_of_key(g.name));
    out.attribution.owner = format!("m{}:global{}", mid.index(), u32::from(gid.into_raw()));
    out.attribution.source = BodySource::Global(mid, gid);
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
            let unit = UnitBody {
                mid,
                owner: BodyOwner::Global(gid),
                symbol: &init_name,
                check_name: g.name.as_str(),
                source_name: g.name.as_str(),
                span: g.name_span,
            };
            let Some(mut lua) = unit.emit(ws, scx, cg, &mut mf, &mut out) else {
                return out;
            };
            let key = Ustr::from(&init_name);
            // A single returned expression is evaluated in the init line itself.
            let returned = lua
                .strip_prefix(&format!("function {init_name}()\n do return "))
                .and_then(|rest| rest.strip_suffix(" end\nend\n"))
                .filter(|expr| !expr.contains('\n'))
                .map(str::to_string);
            let init = match &returned {
                Some(expr) => expr.clone(),
                None => format!("{init_name}()"),
            };
            // An unread constant is still lowered: a `@wire(onchange=...)` dependency
            // compares its body fingerprint.
            let unread = g.is_const && cg.is_unread_constant(g.name);
            if returned.is_some() || unread {
                lua.clear();
            }
            // Read after closure conversion: a lambda's global reads run when it is
            // called, so they are not init-ordering constraints.
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
            if unread {
                return out;
            }
            let target = match global_init_target(ws, scx, cg, g) {
                Ok(target) => target,
                Err(e) => {
                    out.hard_error = Some(e);
                    return out;
                }
            };
            // A `const` init runs unguarded so an edited literal shows on reload.
            let line = if g.is_const {
                format!("{target} = {init}\n")
            } else {
                match global_init_line(ws, scx, cg, g, &init) {
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
