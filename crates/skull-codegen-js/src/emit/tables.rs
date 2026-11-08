//! Runtime type tables: enum singletons, equality, inspect and column matrices.

use super::*;

/// Boxed-enum singleton / names-table defs the emitted bodies read, in name
/// order (deterministic; `var` redeclare is harmless on concat).
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

/// Per-type `__rock_vt_eq` registry for every GC-carrying value struct the
/// program constructs (closed transitively over GC-carrying value-struct fields).
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

/// Per-type `__rock_inspect_ty` registry for every realized struct, closed over
/// all declared struct fields.
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

/// Per-enum `__rock_inspect_ty` renderers for every enum stored as a scalar
/// tag box (`{__ty, __tag, _<slot>..}`). The box carries no variant name, so
/// the name is baked per tag at compile time, matching luajit's per-enum
/// cdata `__tostring` (`skull::enum_cdata_metatypes`): a unit variant renders
/// `E::V`, a tuple `E::V { [1] = .. }`, a record `E::V { f = .. }` with its
/// fields sorted. Name-sorted, emitted from declarations.
pub(super) fn emit_inspect_cdata_enums(ws: &Workspace) -> Result<String, CompilerError> {
    use bone_core::hir::VariantPayload;

    let mut names: Vec<ustr::Ustr> = ws.enums_by_name.keys().copied().collect();
    names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let mut out = String::new();
    for name in names {
        if skull_mir::lower::EnumPlan::for_name(ws, name)
            .and_then(|plan| plan.cdata().cloned())
            .is_none()
        {
            continue;
        }
        let Some(eid) = ws.enums_by_name.get(&name) else {
            continue;
        };
        let ed = &ws.modules[eid.module.index()].enums[eid.def];
        if out.is_empty() {
            out.push_str("var __rock_inspect_ty = __rock_inspect_ty || {};\n");
        }
        let mut body = String::from("var g = t.__tag; ");
        for (tag, variant) in ed.variants.iter().enumerate() {
            let head = format!("{name}::{}", variant.name);
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
                js_string_lit(&head)?
            } else {
                let mut parts = vec![js_string_lit(&format!("{head} {{ "))?];
                for (i, (label, slot)) in slots.iter().enumerate() {
                    let sep = if i == 0 { "" } else { ", " };
                    parts.push(js_string_lit(&format!("{sep}{label} = "))?);
                    parts.push(format!("lua_tostring(t[{}])", js_string_lit(slot)?));
                }
                parts.push(js_string_lit(" }")?);
                parts.join(" + ")
            };
            body.push_str(&format!("if (g === {tag}) return {render}; "));
        }
        body.push_str("return \"?\";");
        out.push_str(&format!(
            "__rock_inspect_ty[{}] = function(t) {{ {body} }};\n",
            js_string_lit(name.as_str())?
        ));
    }
    Ok(out)
}

/// Per-type, per-variant descriptors for flat enum tuple payloads whose
/// scalarized nominal value has a direct record-field walk. They are emitted
/// from declarations, independently of which enum constructors need singleton
/// globals, so an inspect reached through a flattened field always has metadata.
pub(super) fn emit_inspect_enum_payloads(ws: &Workspace) -> Result<String, CompilerError> {
    use bone_core::hir::{TypeCtor, TypeExpr, VariantPayload};
    use bone_core::prelude::TyId;

    let mut names: Vec<ustr::Ustr> = ws.enums_by_name.keys().copied().collect();
    names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let mut out = String::new();
    for name in names {
        let Some(flat) =
            skull_mir::lower::EnumPlan::for_name(ws, name).and_then(|plan| plan.flat().cloned())
        else {
            continue;
        };
        let Some(eid) = ws.enums_by_name.get(&name) else {
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
                    let sid = ws.struct_by_name.get(&child)?;
                    let structure = &ws.modules[sid.module.index()].structs[sid.def];
                    // Lua's enum payload-name helper refuses a record walk
                    // when any declared field type is itself nominal, even if
                    // that field happens to occupy one direct scalar slot.
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
        let enum_name = js_string_lit(name.as_str())?;
        out.push_str(&format!(
            "__rock_enum_inspect_payloads[{enum_name}] = __rock_enum_inspect_payloads[{enum_name}] || Object.create(null);\n"
        ));
        for (variant_name, positions) in variants {
            let variant_name = js_string_lit(variant_name.as_str())?;
            let positions = positions
                .into_iter()
                .map(|fields| match fields {
                    Some(fields) => fields
                        .iter()
                        .map(|field| js_string_lit(field.as_str()))
                        .collect::<Result<Vec<_>, _>>()
                        .map(|fields| format!("[{}]", fields.join(", "))),
                    None => Ok("null".to_string()),
                })
                .collect::<Result<Vec<_>, CompilerError>>()?;
            out.push_str(&format!(
                "__rock_enum_inspect_payloads[{enum_name}][{variant_name}] = [{}];\n",
                positions.join(", ")
            ));
        }
    }
    Ok(out)
}

/// Per-type `__rock_col_mat` registry for every element type the program stores
/// as columns: the JS half of luajit's `__rock_materialize_column_type`, which
/// reads `_G.__ROCK_TYPE_TABLE` for the field walk. JS has no type table and is
/// not getting one (luajit's is slated for deletion),
/// and the element type is a compile-time fact at every column site anyway --
/// that is exactly why `spec_key_has_target_storage` forces specialization for one.
pub(super) fn emit_col_mat(
    ws: &Workspace,
    registry: Option<&skull_mir::lower::StructRegistryPlan>,
) -> Result<String, CompilerError> {
    let Some(registry) = registry else {
        return Ok(String::new());
    };
    build_col_mat_defs(ws, registry.column_plans(), registry.arena())
}

/// Emit `__rock_col_mat["<E>"] = function(v, i) { return {...}; };` -- one
/// element rebuilt from its leaf columns, nested objects and all.
///
/// The leaf paths come from the owned representation plan, THE column-key source (the
/// lowering writes the same paths), so the reader cannot drift from the writer.
/// A column element is a GC-free `value` struct by construction
/// (`vec_column_elem`), so the rebuilt object is a plain `{__ty, ..}` -- the same
/// shape `MakeValueStruct` renders -- and inspect's value tier renders it
/// `E { x: 1, y: 2 }` with no further help.
fn build_col_mat_defs(
    ws: &Workspace,
    plans: &std::collections::BTreeMap<ustr::Ustr, (bool, skull_mir::lower::RepId)>,
    arena: &skull_mir::lower::RepArena,
) -> Result<String, CompilerError> {
    let mut out = String::new();
    for (name, (nobox, representation)) in plans {
        // A `nobox` element has no materializer BY CONSTRUCTION: every op that
        // would need one (inspect, sort, the Lua-array bridge) is a spanned
        // rejection in the lowering, so emitting one would be unreachable code
        // that reads as an escape hatch from the guarantee.
        if *nobox {
            continue;
        }
        let plan = arena.try_get(*representation).ok_or_else(|| {
            err(format!(
                "JS subset: column element `{name}` carries a foreign representation plan"
            ))
        })?;
        let Some(paths) = plan.leaf_paths() else {
            // An enum value-shape has leaf paths but no struct declaration to
            // rebuild against: its `{__tag, _n}` leaves cannot reproduce the
            // named variant a JS enum box carries. Same honest rejection the
            // static `inspect` of a scalarized enum already makes.
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
            js_string_lit(name.as_str())?
        ));
    }
    Ok(out)
}

/// The object literal rebuilding `name` from the columns under `prefix`.
/// Recurses on a dotted path exactly as `render_leaf_form_value_struct` does --
/// same grouping rule, different leaf source (a column read, not a MIR value).
fn col_mat_object(
    ws: &Workspace,
    name: ustr::Ustr,
    paths: &[ustr::Ustr],
    prefix: &str,
) -> Result<String, CompilerError> {
    let mut top: Vec<(ustr::Ustr, String)> = Vec::new();
    let mut groups: Vec<(ustr::Ustr, Vec<ustr::Ustr>)> = Vec::new();
    for p in paths {
        match p.as_str().split_once('.') {
            None => {
                let col = js_string_lit(&format!("{prefix}{p}"))?;
                top.push((*p, format!("v[{col}][i]")));
            }
            Some((head, rest)) => {
                let (head, rest) = (ustr::Ustr::from(head), ustr::Ustr::from(rest));
                match groups.iter_mut().find(|(h, _)| *h == head) {
                    Some((_, sub)) => sub.push(rest),
                    None => groups.push((head, vec![rest])),
                }
            }
        }
    }
    let mut parts = vec![format!("__ty: {}", js_string_lit(name.as_str())?)];
    for (f, read) in &top {
        parts.push(format!("{}: {read}", js_key(f)?));
    }
    for (head, sub) in &groups {
        let child = nested_value_field_struct_name(ws, name, head.as_str())?.ok_or_else(|| {
            err(format!(
                "JS subset: column element `{name}` field `{head}` is not a value struct"
            ))
        })?;
        let inner = col_mat_object(ws, child, sub, &format!("{prefix}{head}."))?;
        parts.push(format!("{}: {inner}", js_key(head)?));
    }
    Ok(format!("({{{}}})", parts.join(", ")))
}

/// The nominal name of `parent`'s `field`, when that field is a struct.
fn nested_value_field_struct_name(
    ws: &Workspace,
    parent: ustr::Ustr,
    field: &str,
) -> Result<Option<ustr::Ustr>, CompilerError> {
    let sid = ws
        .struct_by_name
        .get(&parent)
        .ok_or_else(|| err(format!("column element `{parent}` has no declared struct")))?;
    let s = &ws.module(sid.module).structs[sid.def];
    let Some(f) = s.fields.iter().find(|f| f.name.as_str() == field) else {
        return Err(err(format!("struct `{parent}` has no field `{field}")));
    };
    let plan = skull_mir::lower::struct_field_plan(
        ws,
        bone_core::hir::FnBackend::Js,
        parent,
        f.name,
        false,
    )
    .map_err(|e| err(e.message))?;
    Ok(plan.child().filter(|child| {
        ws.struct_by_name
            .get(child)
            .is_some_and(|sid| ws.module(sid.module).structs[sid.def].is_value)
    }))
}

/// The entry module's nullary `main`, if it defines one -- the program entry.
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

/// A `self_ref` for inlining a body that is NOT a function (a `deco` def): any
/// workspace `FuncRef` the body does not itself call, so [`inline_functions_with_plans`]'s
/// self-call skip (`fref != self_ref`) never suppresses a callee the body wants
/// inlined. Deco bodies never call a decorated user fn, but computing the actual
/// call set makes the choice provably safe regardless.
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

/// All valid `__rock_enum_*` global names -> their JS def line. A payload-free
/// (all-unit, non-cdata) enum yields `__rock_enum_<E>_names`, a `Map` keyed by
/// ordinal (matching the `kind: Map` index the `{x}` stringify lowers to); a mixed
/// enum yields a `__rock_enum_<E>_<V>` singleton per unit variant. cdata-eligible
/// enums have neither (they lower to value structs). Mirrors the Lua backend's
/// `enum_runtime_defs`; only referenced names are emitted, and the defs tolerate
/// duplication across concatenated per-test programs (`var` redeclare, same value).
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
            let symbol = skull_mir::mangle::type_symbol(ws, e.name);
            if all_unit {
                let name = ustr::Ustr::from(format!("__rock_enum_{symbol}_names").as_str());
                let items: Vec<String> = e
                    .variants
                    .iter()
                    .enumerate()
                    .map(|(ord, v)| format!("[{ord}, \"{e}::{v}\"]", e = e.name, v = v.name))
                    .collect();
                out.insert(
                    name,
                    format!("var {name} = new Map([{}]);\n", items.join(", ")),
                );
            } else {
                for v in e.variants.iter() {
                    if matches!(v.payload, VariantPayload::Unit) {
                        let name =
                            ustr::Ustr::from(format!("__rock_enum_{}_{}", symbol, v.name).as_str());
                        out.insert(
                            name,
                            format!(
                                "var {name} = {{ __variant: \"{var}\", __ty: \"{ty}\" }};\n",
                                var = v.name,
                                ty = e.name,
                            ),
                        );
                    }
                }
            }
        }
    }
    out
}

/// A JS expression rebuilding an `embed` field's LOGICAL value from the
/// parent's physical leaves (the port of `__rock_embed_view`): the child has no
/// box of its own, its leaves live at `<prefix>.<subfield>` recursively, and a
/// nullable embed keeps presence at `<prefix>` so an absent one reads as null.
fn embed_view_expr(
    ws: &Workspace,
    prefix: &str,
    ty_name: ustr::Ustr,
    nullable: bool,
) -> Result<String, CompilerError> {
    let sid = ws
        .struct_by_name
        .get(&ty_name)
        .ok_or_else(|| err(format!("embed view `{ty_name}` has no declared struct")))?;
    let s = &ws.module(sid.module).structs[sid.def];
    let mut parts = vec![format!("__ty: {}", js_string_lit(ty_name.as_str())?)];
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
        let val = match plan.embed() {
            Some((child, child_nullable)) => embed_view_expr(ws, &key, child, child_nullable)?,
            _ => format!("t[{}]", js_string_lit(&key)?),
        };
        parts.push(format!("{}: {val}", js_string_lit(f.name.as_str())?));
    }
    let built = format!("{{{}}}", parts.join(", "));
    if !nullable {
        return Ok(built);
    }
    let p = js_string_lit(prefix)?;
    Ok(format!(
        "((t[{p}] === null || t[{p}] === undefined) ? null : {built})"
    ))
}

/// Emit the `__rock_inspect_ty["<N>"] = function(t, ..) {...}` entries: the
/// per-type half of `inspect` that luajit reads out of `_G.__ROCK_TYPE_TABLE`
/// (the declared field list) and `_G.__ROCK_C_STRUCT` (the value-struct
/// renderer). Two tiers, exactly as luajit splits them: an FFI-clean `value`
/// struct is a cdata there, rendered `N { f: v, .. }` in declaration order;
/// everything else (reference structs, GC-carrying `value` structs, which are
/// plain tables on luajit too) renders `N { f = v, .. }` with sorted keys and
/// the compact/multiline threshold.
fn build_inspect_ty_defs(
    ws: &Workspace,
    arena: &skull_mir::lower::RepArena,
    plans: &std::collections::BTreeMap<ustr::Ustr, skull_mir::lower::RepId>,
    names: &BTreeSet<ustr::Ustr>,
) -> Result<String, CompilerError> {
    // Self-declaring: `JS_INSPECT_RUNTIME` also declares the table, but a caller
    // that assembles a program without it (the emitter's own unit tests, which
    // pull only the consts their assertion needs) would otherwise get defs
    // assigning into an undeclared name. `|| {}` keeps the runtime's table when
    // the prelude did supply one -- it always precedes the emitted program.
    let mut out = String::new();
    for name in names {
        let sid = ws.struct_by_name.get(name).ok_or_else(|| {
            err(format!(
                "JS subset: inspected registry type `{name}` is not declared"
            ))
        })?;
        let s = &ws.module(sid.module).structs[sid.def];
        if out.is_empty() {
            out.push_str("var __rock_inspect_ty = __rock_inspect_ty || {};\n");
        }
        let header = js_string_lit(name.as_str())?;
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
        if value_plan
            .as_ref()
            .is_some_and(|plan| plan.value_storage() == Some(skull_mir::lower::ValueStorage::CData))
        {
            let pairs: Vec<String> = s
                .fields
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
                    let n = js_string_lit(f.name.as_str())?;
                    Ok(format!("[{n}, t[{n}]]"))
                })
                .collect::<Result<_, CompilerError>>()?;
            out.push_str(&format!(
                "__rock_inspect_ty[{header}] = function(t, indent, path, visited, precision) {{ \
                 return __rock_inspect_value_struct({header}, [{}]); }};\n",
                pairs.join(", ")
            ));
            continue;
        }
        let mut fields: Vec<&bone_core::hir::StructFieldDef> = s.fields.iter().collect();
        fields.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        let pairs: Vec<String> = fields
            .iter()
            .map(|f| {
                let n = js_string_lit(f.name.as_str())?;
                let plan = skull_mir::lower::struct_field_plan(
                    ws,
                    bone_core::hir::FnBackend::Js,
                    *name,
                    f.name,
                    false,
                )
                .map_err(|e| err(e.message))?;
                let val = match plan.embed() {
                    Some((child, nullable)) => format!(
                        "((t[{n}] !== null && typeof t[{n}] === \"object\") ? t[{n}] : {})",
                        embed_view_expr(ws, f.name.as_str(), child, nullable)?
                    ),
                    None => format!("t[{n}]"),
                };
                Ok(format!("[{n}, {val}]"))
            })
            .collect::<Result<_, CompilerError>>()?;
        out.push_str(&format!(
            "__rock_inspect_ty[{header}] = function(t, indent, path, visited, precision) {{ \
             return __rock_inspect_struct({header}, t, [{}], indent, path, visited, precision); }};\n",
            pairs.join(", ")
        ));
    }
    Ok(out)
}

/// Emit the `__rock_vt_eq["<N>"] = function(a, b) {...}` entries mirroring the
/// Lua `_vt.__eq` (`skull::value_struct_gc_defs`) field-for-field: `__op_eq`
/// (structural) where `gc_field_eq` selects contents equality -- scalars, enums,
/// nested value structs; `===` (by reference, luajit's `rawequal`) for a Vec /
/// Map / Arena / reference-struct / generic-`T` / string field. `===` on a JS
/// string is by-value, matching luajit's interning.
pub(super) fn build_vt_eq_defs(
    ws: &Workspace,
    names: &BTreeMap<ustr::Ustr, skull_mir::lower::ValueStorage>,
) -> Result<String, CompilerError> {
    let mut out = String::new();
    for (name, storage) in names {
        let sid = ws.struct_by_name.get(name).ok_or_else(|| {
            err(format!(
                "JS subset: value equality registry type `{name}` is not declared"
            ))
        })?;
        let s = &ws.module(sid.module).structs[sid.def];
        let compare =
            |key: &str, equality: skull_mir::lower::GcFieldEq| -> Result<String, CompilerError> {
                let (a, b) = if key.contains('.') {
                    (
                        format!("a[{}]", js_string_lit(key)?),
                        format!("b[{}]", js_string_lit(key)?),
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
        // A GC-table box keys an inline child's leaves by dotted path.
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
            js_string_lit(name.as_str())?
        ));
    }
    Ok(out)
}
