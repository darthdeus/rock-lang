//! Runtime type tables: enum singletons, equality, inspect and column matrices.

use super::*;

pub(super) fn emit_enum_singletons(
    gctx: &GlobalCtx,
    enum_defs: &HashMap<ustr::Ustr, String>,
) -> String {
    let mut enum_names: Vec<&ustr::Ustr> = gctx.enum_globals.iter().collect();
    enum_names.sort();
    let mut out = String::new();
    for name in enum_names {
        out.push_str(&enum_defs[name]);
    }
    out
}

pub(super) fn emit_vt_eq(
    ws: &Workspace,
    registry: Option<&skull_mir::lower::StructRegistryPlan>,
) -> Result<String, CompilerError> {
    let Some(registry) = registry else {
        return Ok(String::new());
    };
    let arena = registry.arena();
    let mut vt_names: BTreeMap<ustr::Ustr, skull_mir::lower::ValueStorage> = BTreeMap::new();
    for (&name, &representation) in registry.value_plans() {
        let plan = arena.try_get(representation).ok_or_else(|| {
            err(format!(
                "JS subset: value equality registry `{name}` carries a foreign representation plan"
            ))
        })?;
        match plan.value_storage() {
            Some(skull_mir::lower::ValueStorage::CData) | None => {}
            Some(storage) => {
                vt_names.insert(name, storage);
            }
        }
    }
    build_vt_eq_defs(ws, &vt_names)
}

/// Per-type `__rock_tostring_ty` renderers for every realized value struct, matching luajit's
/// `__tostring`: `Name { f: v, .. }` with each leaf through `lua_tostring`.
pub(super) fn emit_tostring_ty(
    ws: &Workspace,
    registry: Option<&skull_mir::lower::StructRegistryPlan>,
) -> Result<String, CompilerError> {
    use skull_mir::lower::ValueStorage;
    use skull_mir::lua_nesting::{LuaDisplayPart, display_record};

    let Some(registry) = registry else {
        return Ok(String::new());
    };
    let arena = registry.arena();
    let leaf = |key: &str| vec![LuaDisplayPart::Expr(format!("tostring(v[\"{key}\"])"))];
    let mut out = String::new();
    for (&name, &representation) in registry.value_plans() {
        let plan = arena.try_get(representation).ok_or_else(|| {
            err(format!(
                "JS subset: value `{name}` carries a foreign representation plan"
            ))
        })?;
        let Some(storage) = plan.value_storage() else {
            continue;
        };
        let display_name = bone_core::modules::display_name(name.as_str());
        let open = format!("{display_name} {{ ");
        let fields: Vec<(String, Vec<LuaDisplayPart>)> = match storage {
            ValueStorage::CData => {
                let sid = ws.nominals.struct_id(name).ok_or_else(|| {
                    err(format!(
                        "JS subset: value `{name}` is not a declared struct"
                    ))
                })?;
                ws.module(sid.module).structs[sid.def]
                    .fields
                    .iter()
                    .map(|f| (format!("{}: ", f.name), leaf(f.name.as_str())))
                    .collect()
            }
            ValueStorage::LeafTable => arena
                .nominal_value_leaf_paths_required(ws, bone_core::hir::FnBackend::Js, name)
                .map_err(|e| err(e.message))?
                .iter()
                .map(|p| (format!("{p}: "), leaf(p.as_str())))
                .collect(),
            ValueStorage::GcTable => {
                skull_mir::lower::gc_table_fields(ws, bone_core::hir::FnBackend::Js, name)
                    .map_err(|e| err(e.message))?
                    .iter()
                    .map(|f| (format!("{}: ", f.name), f.lua_display_parts()))
                    .collect()
            }
        };
        let parts = if fields.is_empty() {
            vec![LuaDisplayPart::literal(&format!("{display_name} {{ }}"))]
        } else {
            display_record(&open, fields, " }")
        };
        if out.is_empty() {
            out.push_str("var __rock_tostring_ty = __rock_tostring_ty || {};\n");
        }
        out.push_str(&format!(
            "__rock_tostring_ty[{}] = function(v) {{ return {}; }};\n",
            js_string_lit(
                skull_mir::mangle::type_symbol(ws, ustr::Ustr::from(name.as_str())).as_str()
            ),
            js_display_expr(&parts)?
        ));
    }
    Ok(out)
}

/// The JS spelling of a display body shared with the luajit `__tostring` emitter; anything but
/// string literals and `tostring(v.<key>)` reads is rejected.
fn js_display_expr(
    parts: &[skull_mir::lua_nesting::LuaDisplayPart],
) -> Result<String, CompilerError> {
    use skull_mir::lua_nesting::LuaDisplayPart;

    let read = |e: &str| -> Option<String> {
        let inner = e.strip_prefix("tostring(")?.strip_suffix(')')?;
        is_js_box_read(inner).then(|| format!("lua_tostring({inner})"))
    };
    let mut terms = Vec::with_capacity(parts.len());
    for part in parts {
        terms.push(match part {
            LuaDisplayPart::Expr(e)
                if e.len() >= 2
                    && e.starts_with('"')
                    && e.ends_with('"')
                    && !e[1..e.len() - 1].contains(['"', '\\']) =>
            {
                e.clone()
            }
            LuaDisplayPart::Expr(e) => read(e).ok_or_else(|| {
                err(format!(
                    "JS subset: display operand `{e}` has no JS spelling"
                ))
            })?,
            LuaDisplayPart::UnlessNil { value, parts } if is_js_box_read(value) => format!(
                "({value} == null ? \"nil\" : ({}))",
                js_display_expr(parts)?
            ),
            LuaDisplayPart::UnlessNil { value, .. } => {
                return Err(err(format!(
                    "JS subset: display presence read `{value}` has no JS spelling"
                )));
            }
        });
    }
    Ok(terms.join(" + "))
}

fn is_js_box_read(e: &str) -> bool {
    if let Some(key) = e.strip_prefix("v.") {
        return !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    }
    e.strip_prefix("v[\"")
        .and_then(|rest| rest.strip_suffix("\"]"))
        .is_some_and(|key| !key.contains(['"', '\\']))
}

pub(super) fn emit_inspect_ty(
    ws: &Workspace,
    registry: Option<&skull_mir::lower::StructRegistryPlan>,
) -> Result<String, CompilerError> {
    let Some(registry) = registry else {
        return Ok(String::new());
    };
    build_inspect_ty_defs(
        ws,
        registry.arena(),
        registry.value_plans(),
        registry.names(),
    )
}

/// Per-enum `__rock_inspect_ty` renderers for scalar-tag-box enums. The box carries no variant
/// name, so it is baked per tag to match luajit's cdata `__tostring`.
pub(super) fn emit_inspect_cdata_enums(ws: &Workspace) -> String {
    use bone_core::hir::VariantPayload;

    let mut names: Vec<ustr::Ustr> = ws.nominals.enum_names().collect();
    names.sort_by(|a, b| {
        bone_core::modules::display_name(a.as_str())
            .cmp(bone_core::modules::display_name(b.as_str()))
    });
    let mut out = String::new();
    for name in names {
        if skull_mir::lower::EnumPlan::for_name(ws, name)
            .and_then(|plan| plan.cdata().cloned())
            .is_none()
        {
            continue;
        }
        let Some(eid) = ws.nominals.enum_id(name) else {
            continue;
        };
        let ed = &ws.modules[eid.module.index()].enums[eid.def];
        if out.is_empty() {
            out.push_str("var __rock_inspect_ty = __rock_inspect_ty || {};\n");
            out.push_str("var __rock_tostring_ty = __rock_tostring_ty || {};\n");
        }
        let mut body = String::from("var g = t.__tag; ");
        for (tag, variant) in ed.variants.iter().enumerate() {
            let display_name = bone_core::modules::display_name(name.as_str());
            let head = format!("{display_name}::{}", variant.name);
            let slots: Vec<(String, String)> = match &variant.payload {
                VariantPayload::Tuple(types) => (0..types.len())
                    .map(|j| (format!("[{}]", j + 1), format!("_{j}")))
                    .collect(),
                VariantPayload::Record(fields) => {
                    let mut names: Vec<ustr::Ustr> = fields.iter().map(|f| f.name).collect();
                    names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
                    names
                        .iter()
                        .map(|f| (f.to_string(), format!("_{f}")))
                        .collect()
                }
                _ => Vec::new(),
            };
            let render = if slots.is_empty() {
                js_string_lit(&head)
            } else {
                let mut parts = vec![js_string_lit(&format!("{head} {{ "))];
                for (i, (label, slot)) in slots.iter().enumerate() {
                    let sep = if i == 0 { "" } else { ", " };
                    parts.push(js_string_lit(&format!("{sep}{label} = ")));
                    parts.push(format!("lua_tostring(t[{}])", js_string_lit(slot)));
                }
                parts.push(js_string_lit(" }"));
                parts.join(" + ")
            };
            body.push_str(&format!("if (g === {tag}) return {render}; "));
        }
        body.push_str("return \"?\";");
        let key = js_string_lit(
            skull_mir::mangle::type_symbol(ws, ustr::Ustr::from(name.as_str())).as_str(),
        );
        out.push_str(&format!(
            "__rock_inspect_ty[{key}] = function(t) {{ {body} }};\n\
             __rock_tostring_ty[{key}] = __rock_inspect_ty[{key}];\n"
        ));
    }
    out
}

/// Descriptors for flat enum tuple payloads with a direct record-field walk, emitted from
/// declarations so an inspect reached through a flattened field always has metadata.
pub(super) fn emit_inspect_enum_payloads(ws: &Workspace) -> String {
    use bone_core::hir::{TypeCtor, TypeExpr, VariantPayload};
    use bone_core::prelude::TyId;

    let mut names: Vec<ustr::Ustr> = ws.nominals.enum_names().collect();
    names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let mut out = String::new();
    for name in names {
        let Some(flat) =
            skull_mir::lower::EnumPlan::for_name(ws, name).and_then(|plan| plan.flat().cloned())
        else {
            continue;
        };
        let Some(eid) = ws.nominals.enum_id(name) else {
            continue;
        };
        let ed = &ws.modules[eid.module.index()].enums[eid.def];
        let mut variants = Vec::new();
        for variant in &ed.variants {
            let VariantPayload::Tuple(types) = &variant.payload else {
                continue;
            };
            let mut positions = Vec::with_capacity(types.len());
            let mut any_record = false;
            for (position, ty) in types.iter().enumerate() {
                let nominal = match *ty {
                    TyId::Static(module, index) => match ws.module(module).ty_arena[index] {
                        TypeExpr::App {
                            ctor: TypeCtor::Nominal(child),
                            ..
                        } => Some(child),
                        _ => None,
                    },
                    TyId::Local(_) => None,
                };
                let fields = nominal.and_then(|child| {
                    let sid = ws.nominals.struct_id(child)?;
                    let structure = &ws.modules[sid.module.index()].structs[sid.def];
                    // Lua's payload-name helper refuses a record walk when any declared field type
                    // is nominal, even if it occupies one scalar slot.
                    let has_nominal_field = structure.fields.iter().any(|field| match field.ty {
                        TyId::Static(module, index) => matches!(
                            ws.module(module).ty_arena[index],
                            TypeExpr::App {
                                ctor: TypeCtor::Nominal(_),
                                ..
                            }
                        ),
                        TyId::Local(_) => false,
                    });
                    if !structure.is_value || structure.fields.is_empty() || has_nominal_field {
                        return None;
                    }
                    let leaves = flat.position_leaf_paths(position, child)?;
                    if leaves.len() != structure.fields.len()
                        || leaves
                            .iter()
                            .zip(&structure.fields)
                            .any(|(leaf, field)| leaf.as_str().contains('.') || *leaf != field.name)
                    {
                        return None;
                    }
                    Some(leaves)
                });
                if fields.is_some() {
                    any_record = true;
                }
                positions.push(fields);
            }
            if any_record {
                variants.push((variant.name, positions));
            }
        }
        if variants.is_empty() {
            continue;
        }
        if out.is_empty() {
            out.push_str(
                "var __rock_enum_inspect_payloads = __rock_enum_inspect_payloads || Object.create(null);\n",
            );
        }
        let enum_name = js_string_lit(
            skull_mir::mangle::type_symbol(ws, ustr::Ustr::from(name.as_str())).as_str(),
        );
        out.push_str(&format!(
            "__rock_enum_inspect_payloads[{enum_name}] = __rock_enum_inspect_payloads[{enum_name}] || Object.create(null);\n"
        ));
        for (variant_name, positions) in variants {
            let variant_name = js_string_lit(variant_name.as_str());
            let positions = positions
                .into_iter()
                .map(|fields| match fields {
                    Some(fields) => format!(
                        "[{}]",
                        fields
                            .iter()
                            .map(|field| js_string_lit(field.as_str()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    None => "null".to_string(),
                })
                .collect::<Vec<_>>();
            out.push_str(&format!(
                "__rock_enum_inspect_payloads[{enum_name}][{variant_name}] = [{}];\n",
                positions.join(", ")
            ));
        }
    }
    out
}

pub(super) fn emit_col_mat(
    ws: &Workspace,
    registry: Option<&skull_mir::lower::StructRegistryPlan>,
) -> Result<String, CompilerError> {
    let Some(registry) = registry else {
        return Ok(String::new());
    };
    build_col_mat_defs(ws, registry.column_plans(), registry.arena())
}

/// `__rock_col_mat["<E>"] = function(v, i) { return {...}; };` rebuilds one element from its leaf
/// columns. The leaf paths come from the owned representation plan, the same source the lowering
/// writes.
fn build_col_mat_defs(
    ws: &Workspace,
    plans: &std::collections::BTreeMap<ustr::Ustr, (bool, skull_mir::lower::RepId)>,
    arena: &skull_mir::lower::RepArena,
) -> Result<String, CompilerError> {
    let mut out = String::new();
    for (name, (nobox, representation)) in plans {
        // A `nobox` element has no materializer: every op needing one is already rejected in
        // lowering.
        if *nobox {
            continue;
        }
        let plan = arena.try_get(*representation).ok_or_else(|| {
            err(format!(
                "JS subset: column element `{name}` carries a foreign representation plan"
            ))
        })?;
        let Some(paths) = plan.leaf_paths() else {
            // An enum value-shape has no struct declaration to rebuild against; reject like the
            // static `inspect` of a scalarized enum.
            return Err(err(format!(
                "JS subset: column element `{name}` is not a value struct \
                 (no declared shape to rebuild an element from)"
            )));
        };
        if out.is_empty() {
            out.push_str("var __rock_col_mat = __rock_col_mat || {};\n");
        }
        let body = col_mat_object(ws, *name, &paths, "")?;
        out.push_str(&format!(
            "__rock_col_mat[{}] = function(v, i) {{ return {body}; }};\n",
            js_string_lit(
                skull_mir::mangle::type_symbol(ws, ustr::Ustr::from(name.as_str())).as_str()
            )
        ));
    }
    Ok(out)
}

fn col_mat_object(
    ws: &Workspace,
    name: ustr::Ustr,
    paths: &[ustr::Ustr],
    prefix: &str,
) -> Result<String, CompilerError> {
    let (top, groups) = split_leaf_paths(paths.iter().map(|p| (*p, ())));
    let mut parts = vec![format!(
        "__ty: {}",
        js_string_lit(skull_mir::mangle::type_symbol(ws, ustr::Ustr::from(name.as_str())).as_str())
    )];
    for (f, ()) in &top {
        let col = js_string_lit(&format!("{prefix}{f}"));
        parts.push(format!("{}: v[{col}][i]", js_key(f)));
    }
    for (head, sub) in &groups {
        let child = nested_field_struct_name(ws, name, head.as_str())?
            .filter(|child| {
                ws.nominals
                    .struct_id(*child)
                    .is_some_and(|sid| ws.module(sid.module).structs[sid.def].is_value)
            })
            .ok_or_else(|| {
                err(format!(
                    "JS subset: column element `{name}` field `{head}` is not a value struct"
                ))
            })?;
        let sub: Vec<ustr::Ustr> = sub.iter().map(|(p, ())| *p).collect();
        let inner = col_mat_object(ws, child, &sub, &format!("{prefix}{head}."))?;
        parts.push(format!("{}: {inner}", js_key(head)));
    }
    Ok(format!("({{{}}})", parts.join(", ")))
}

pub(super) fn split_leaf_paths<T>(
    fields: impl IntoIterator<Item = (ustr::Ustr, T)>,
) -> (
    Vec<(ustr::Ustr, T)>,
    Vec<(ustr::Ustr, Vec<(ustr::Ustr, T)>)>,
) {
    let mut top = Vec::new();
    let mut groups: Vec<(ustr::Ustr, Vec<(ustr::Ustr, T)>)> = Vec::new();
    for (key, value) in fields {
        match key.as_str().split_once('.') {
            None => top.push((key, value)),
            Some((head, rest)) => {
                let (head, rest) = (ustr::Ustr::from(head), ustr::Ustr::from(rest));
                match groups.iter_mut().find(|(h, _)| *h == head) {
                    Some((_, sub)) => sub.push((rest, value)),
                    None => groups.push((head, vec![(rest, value)])),
                }
            }
        }
    }
    (top, groups)
}

pub(super) fn nested_field_struct_name(
    ws: &Workspace,
    parent: ustr::Ustr,
    field: &str,
) -> Result<Option<ustr::Ustr>, CompilerError> {
    let sid = ws
        .nominals
        .struct_id(parent)
        .ok_or_else(|| err(format!("value `{parent}` has no declared struct")))?;
    let s = &ws.module(sid.module).structs[sid.def];
    let Some(f) = s.fields.iter().find(|f| f.name.as_str() == field) else {
        return Err(err(format!("struct `{parent}` has no field `{field}`")));
    };
    let plan = skull_mir::lower::struct_field_plan(
        ws,
        bone_core::hir::FnBackend::Js,
        parent,
        f.name,
        false,
    )
    .map_err(|e| err(e.message))?;
    Ok(plan
        .child()
        .filter(|child| ws.nominals.struct_id(*child).is_some()))
}

/// Matched by declared name: a modules-v1 entry's key is `<package>::main`.
pub(super) fn nullary_main(
    ws: &Workspace,
    user_mid: bone_core::prelude::ModuleId,
) -> Option<FuncRef> {
    ws.module(user_mid)
        .functions
        .iter()
        .find(|(_, f)| {
            skull_mir::mangle::declared_name(f.name.as_str()) == "main" && f.param_decls.is_empty()
        })
        .map(|(fid, _)| FuncRef {
            module: user_mid,
            func: fid,
        })
}

/// A `self_ref` for inlining a non-function body (a `deco` def): any `FuncRef` the body does not
/// itself call, so the self-call skip never suppresses a wanted callee.
pub(super) fn safe_self_ref(ws: &Workspace, mf: &MirFunc) -> FuncRef {
    let targets: HashSet<FuncRef> = skull_mir::reach::call_targets(mf).into_iter().collect();
    ws.source_modules()
        .flat_map(|m| {
            m.functions.iter().map(move |(fid, _)| FuncRef {
                module: m.id,
                func: fid,
            })
        })
        .find(|fr| !targets.contains(fr))
        .expect("workspace has a function the deco body does not call")
}

/// Valid `__rock_enum_*` names -> JS def line: `__rock_enum_<E>_names` (a `Map` keyed by ordinal)
/// for an all-unit non-cdata enum, a `__rock_enum_<E>_<V>` singleton per unit variant of a mixed
/// enum. Defs tolerate duplication across concatenated per-test programs.
pub(super) fn build_enum_global_defs(ws: &Workspace) -> HashMap<ustr::Ustr, String> {
    use bone_core::hir::VariantPayload;
    let mut out = HashMap::new();
    for module in ws.modules.iter() {
        for (_, e) in module.enums.iter() {
            if skull_mir::lower::EnumPlan::for_name(ws, e.name)
                .is_some_and(|plan| plan.cdata().is_some())
            {
                continue;
            }
            let all_unit = !e.variants.is_empty()
                && e.variants
                    .iter()
                    .all(|v| matches!(v.payload, VariantPayload::Unit));
            if all_unit {
                let name = skull_mir::mangle::enum_names_global(ws, e.name);
                let display_name = bone_core::modules::display_name(e.name.as_str());
                let items: Vec<String> = e
                    .variants
                    .iter()
                    .enumerate()
                    .map(|(ord, v)| format!("[{ord}, \"{display_name}::{}\"]", v.name))
                    .collect();
                out.insert(
                    name,
                    format!("var {name} = new Map([{}]);\n", items.join(", ")),
                );
            } else {
                for v in e.variants.iter() {
                    if matches!(v.payload, VariantPayload::Unit) {
                        let name = skull_mir::mangle::enum_singleton_global(ws, e.name, v.name);
                        out.insert(
                            name,
                            format!(
                                "var {name} = {{ __variant: \"{var}\", __ty: \"{ty}\" }};\n",
                                var = v.name,
                                ty = skull_mir::mangle::type_symbol(ws, e.name),
                            ),
                        );
                    }
                }
            }
        }
    }
    out
}

/// Rebuild an `embed` field's logical value from the parent's physical leaves
/// (`__rock_embed_view`); a nullable embed keeps presence at `<prefix>`.
fn embed_view_expr(
    ws: &Workspace,
    prefix: &str,
    ty_name: ustr::Ustr,
    nullable: bool,
) -> Result<String, CompilerError> {
    let sid = ws
        .nominals
        .struct_id(ty_name)
        .ok_or_else(|| err(format!("embed view `{ty_name}` has no declared struct")))?;
    let s = &ws.module(sid.module).structs[sid.def];
    let mut parts = vec![format!(
        "__ty: {}",
        js_string_lit(skull_mir::mangle::type_symbol(ws, ty_name).as_str())
    )];
    for f in &s.fields {
        let key = format!("{prefix}.{}", f.name);
        let plan = skull_mir::lower::struct_field_plan(
            ws,
            bone_core::hir::FnBackend::Js,
            ty_name,
            f.name,
            true,
        )
        .map_err(|e| err(e.message))?;
        let val =
            inline_view_expr(ws, &key, &plan)?.unwrap_or(format!("t[{}]", js_string_lit(&key)));
        parts.push(format!("{}: {val}", js_string_lit(f.name.as_str())));
    }
    let built = format!("{{{}}}", parts.join(", "));
    if !nullable {
        return Ok(built);
    }
    let p = js_string_lit(prefix);
    Ok(format!(
        "((t[{p}] === null || t[{p}] === undefined) ? null : {built})"
    ))
}

fn inline_view_expr(
    ws: &Workspace,
    prefix: &str,
    plan: &skull_mir::lower::StructFieldPlan,
) -> Result<Option<String>, CompilerError> {
    if let Some((child, nullable)) = plan.embed() {
        return embed_view_expr(ws, prefix, child, nullable).map(Some);
    }
    let skull_mir::lower::StructFieldPlan::Inline {
        flat: Some(shape), ..
    } = plan
    else {
        return Ok(None);
    };
    let tag = format!("t[{}]", js_string_lit(&format!("{prefix}.#")));
    let mut payload = Vec::new();
    for i in 0..shape.tuple_width() {
        payload.push(format!(
            "{i}, t[{}]",
            js_string_lit(&format!("{prefix}.{i}"))
        ));
    }
    for field in shape.record_fields() {
        payload.push(format!(
            "{}, t[{}]",
            js_string_lit(field.as_str()),
            js_string_lit(&format!("{prefix}.{field}"))
        ));
    }
    Ok(Some(format!(
        "({tag} == null ? null : __rock_make_enum({tag}, {}, [{}]))",
        js_string_lit(skull_mir::mangle::type_symbol(ws, shape.name()).as_str()),
        payload.join(", ")
    )))
}

fn build_inspect_ty_defs(
    ws: &Workspace,
    arena: &skull_mir::lower::RepArena,
    plans: &std::collections::BTreeMap<ustr::Ustr, skull_mir::lower::RepId>,
    names: &BTreeSet<ustr::Ustr>,
) -> Result<String, CompilerError> {
    let mut out = String::new();
    for name in names {
        let sid = ws.nominals.struct_id(*name).ok_or_else(|| {
            err(format!(
                "JS subset: inspected registry type `{name}` is not declared"
            ))
        })?;
        let s = &ws.module(sid.module).structs[sid.def];
        if out.is_empty() {
            out.push_str("var __rock_inspect_ty = __rock_inspect_ty || {};\n");
        }
        let header = js_string_lit(
            skull_mir::mangle::type_symbol(ws, ustr::Ustr::from(name.as_str())).as_str(),
        );
        let display_header = js_string_lit(bone_core::modules::display_name(name.as_str()));
        let value_plan = if skull_mir::lower::has_value_plan(s) {
            let representation = plans.get(name).ok_or_else(|| {
                err(format!(
                    "JS subset: inspected value `{name}` has no owned representation plan"
                ))
            })?;
            Some(arena.try_get(*representation).ok_or_else(|| {
                err(format!(
                    "JS subset: inspected value `{name}` carries a foreign representation plan"
                ))
            })?)
        } else {
            None
        };
        let mut fields: Vec<&bone_core::hir::StructFieldDef> = s.fields.iter().collect();
        fields.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        if value_plan
            .as_ref()
            .is_some_and(|plan| plan.value_storage() == Some(skull_mir::lower::ValueStorage::CData))
        {
            let pairs: Vec<String> = fields
                .iter()
                .map(|f| {
                    let plan = skull_mir::lower::struct_field_plan(
                        ws,
                        bone_core::hir::FnBackend::Js,
                        *name,
                        f.name,
                        false,
                    )
                    .map_err(|e| err(e.message))?;
                    if !plan.is_direct() {
                        return Err(err(format!(
                            "JS subset: cdata value `{name}` field `{}` has non-direct field plan",
                            f.name
                        )));
                    }
                    let n = js_string_lit(f.name.as_str());
                    Ok(format!("[{n}, t[{n}]]"))
                })
                .collect::<Result<_, CompilerError>>()?;
            out.push_str(&format!(
                "__rock_inspect_ty[{header}] = function(t, indent, path, visited, precision) {{ \
                 return __rock_inspect_struct({display_header}, t, [{}], indent, path, visited, precision); }};\n",
                pairs.join(", ")
            ));
            continue;
        }
        let pairs: Vec<String> = fields
            .iter()
            .map(|f| {
                let n = js_string_lit(f.name.as_str());
                let plan = skull_mir::lower::struct_field_plan(
                    ws,
                    bone_core::hir::FnBackend::Js,
                    *name,
                    f.name,
                    false,
                )
                .map_err(|e| err(e.message))?;
                let val = match inline_view_expr(ws, f.name.as_str(), &plan)? {
                    Some(view) => format!(
                        "((t[{n}] !== null && typeof t[{n}] === \"object\") ? t[{n}] : {})",
                        view
                    ),
                    None => format!("t[{n}]"),
                };
                Ok(format!("[{n}, {val}]"))
            })
            .collect::<Result<_, CompilerError>>()?;
        out.push_str(&format!(
            "__rock_inspect_ty[{header}] = function(t, indent, path, visited, precision) {{ \
             return __rock_inspect_struct({display_header}, t, [{}], indent, path, visited, precision); }};\n",
            pairs.join(", ")
        ));
    }
    Ok(out)
}

/// `__rock_vt_eq` entries mirroring Lua `_vt.__eq`: `__op_eq` for scalars, enums and nested value
/// structs; `===` (luajit's `rawequal`) for Vec/Map/Arena/reference-struct/generic/string fields.
pub(super) fn build_vt_eq_defs(
    ws: &Workspace,
    names: &BTreeMap<ustr::Ustr, skull_mir::lower::ValueStorage>,
) -> Result<String, CompilerError> {
    let mut out = String::new();
    for (name, storage) in names {
        let sid = ws.nominals.struct_id(*name).ok_or_else(|| {
            err(format!(
                "JS subset: value equality registry type `{name}` is not declared"
            ))
        })?;
        let s = &ws.module(sid.module).structs[sid.def];
        let compare =
            |key: &str, equality: skull_mir::lower::GcFieldEq| -> Result<String, CompilerError> {
                let (a, b) = if key.contains('.') {
                    (
                        format!("a[{}]", js_string_lit(key)),
                        format!("b[{}]", js_string_lit(key)),
                    )
                } else {
                    (format!("a.{key}"), format!("b.{key}"))
                };
                Ok(if equality == skull_mir::lower::GcFieldEq::Reference {
                    format!("{a} === {b}")
                } else {
                    format!("__op_eq({a}, {b})")
                })
            };
        let cmps: Vec<String> = if *storage == skull_mir::lower::ValueStorage::GcTable {
            let fields =
                skull_mir::lower::gc_table_fields(ws, bone_core::hir::FnBackend::Js, *name)
                    .map_err(|e| err(e.message))?;
            let mut cmps = Vec::new();
            for f in &fields {
                for ((key, _), equality) in f.keys().into_iter().zip(f.keys_eq(ws)) {
                    cmps.push(compare(key.as_str(), equality)?);
                }
            }
            cmps
        } else {
            s.fields
                .iter()
                .map(|f| compare(f.name.as_str(), skull_mir::lower::gc_field_eq(ws, f.ty)))
                .collect::<Result<_, _>>()?
        };
        let body = if cmps.is_empty() {
            "true".to_string()
        } else {
            cmps.join(" && ")
        };
        out.push_str(&format!(
            "__rock_vt_eq[{}] = function(a, b) {{ return {body}; }};\n",
            js_string_lit(
                skull_mir::mangle::type_symbol(ws, ustr::Ustr::from(name.as_str())).as_str()
            )
        ));
    }
    Ok(out)
}
