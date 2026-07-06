use bone_semantic::hir::{BindingKind, Function};
use bone_semantic::{hir::NodeRef, prelude::*};
use skull_parser::Ast;

pub fn definition_at(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Option<Span> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;

    // Record-pattern field names aren't in node_index (the cursor there falls
    // through to the arm body); resolve them to the field's declaration.
    if let Some(s) = pattern_field_def(ws, body, offset) {
        return Some(s);
    }

    // Smallest node covering the cursor, preferring one with an INFORMATIVE
    // type (see `hover_at`): keeps goto-definition on the identifier if a small
    // uninformative overlay ever shares its offset.
    let def_bi = inf.per_fn.get(&owner);
    let iv = body
        .node_index()
        .find(offset, offset + 1)
        .min_by_key(|x| (def_bi.map_or(0, |bi| node_ty_uninformative(ws, bi, x.val)), x.stop - x.start))?;

    match iv.val {
        // Cursor on a decl itself
        NodeRef::Binding(b) => body.binding_spans.get(b).copied(),

        NodeRef::Expr(e) => match &body.expr_arena[e] {
            hir::Expr::Var(b) => body.binding_spans.get(*b).copied(),

            hir::Expr::Call { callee, .. } => match callee {
                hir::Callee::Local(b) => body.binding_spans.get(*b).copied(),

                hir::Callee::Unresolved { .. } => {
                    let bi = inf.per_fn.get(&owner)?;
                    let (cmid, fid) = bi.resolved_overload_calls.get(&e)?;

                    Some(ws.modules.get(cmid.index())?.functions[*fid].name_span)
                }

                hir::Callee::Value(_) => None,
            },

            hir::Expr::Global(name) | hir::Expr::SiblingRef(name) => def_of_name(ws, *name),

            // A field name in a struct literal -> the struct's field decl;
            // otherwise the head -> the struct.
            hir::Expr::StructLiteral { name, fields } => {
                for fidx in fields.clone() {
                    let f = &body.struct_lit_field_arena[fidx];
                    let (s, e) = f.name_span.offset_range;
                    if s <= offset && offset < e {
                        return struct_field_span(ws, *name, f.name);
                    }
                }
                def_of_name(ws, *name)
            }

            hir::Expr::EnumVariant { enum_name, .. } => def_of_name(ws, *enum_name),

            // `recv.field` -> the field's declaration in recv's struct.
            hir::Expr::FieldAccess { base, field } => {
                let bi = inf.per_fn.get(&owner)?;
                let base_ty = *bi.expr_types.get(*base)?;
                // struct_app_args resolves through `bi.ty_arena` for inferred
                // (Local) types -- struct_name_of_ty's `lowered_tys` is only the
                // annotation arena and would index out of bounds here.
                let (sname, _) = struct_app_args(ws, bi, base_ty)?;
                struct_field_span(ws, sname, *field)
            }

            _ => None,
        },

        // A type annotation (`p: Point`, `-> Vec<int>`): jump to the named
        // type's declaration. Builtins (`Vec`/`NativeBuffer*` structs, `Map`,
        // `Coroutine`) land on their stdlib definition/landmark.
        NodeRef::StaticTy { mid, idx } => {
            type_def_target(ws, &ws.modules.get(mid.index())?.ty_arena[idx])
        }
        NodeRef::LocalTy(idx) => {
            let bi = inf.per_fn.get(&owner)?;
            type_def_target(ws, &bi.ty_arena[idx])
        }
        NodeRef::Statement(..) => None,
    }
}

/// The declaration of the *type* of the value/binding under `offset` (LSP
/// `textDocument/typeDefinition`). Unlike [`definition_at`], which jumps to where
/// a name is bound, this jumps to where that name's type is declared: on `mob` in
/// `let mob = spawn()` it lands on `struct Mob`. Resolution reads the inferred
/// type of the expression or binding node, peels a leading `?`, and reuses the
/// same nominal/builtin landmark resolution as type-annotation goto-def. `None`
/// for primitives, function types, and anything without a navigable declaration.
pub fn type_definition_at(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Option<Span> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let bi = inf.per_fn.get(&owner)?;

    let iv = body
        .node_index()
        .find(offset, offset + 1)
        .min_by_key(|x| x.stop - x.start)?;

    let ty = match iv.val {
        NodeRef::Binding(b) => *bi.binding_types.get(b)?,
        NodeRef::Expr(e) => *bi.expr_types.get(e)?,
        _ => return None,
    };

    type_def_target_ty(ws, bi, ty)
}

/// [`type_def_target`] for an inferred `TyId`: deref to its `TypeExpr`, peel a
/// `Nullable` wrapper (`Mob?` -> `Mob`), then resolve the named type's
/// declaration.
fn type_def_target_ty(ws: &Workspace, bi: &BodyInference, ty: TyId) -> Option<Span> {
    match deref_ty(ws, Some(bi), ty)? {
        hir::TypeExpr::Nullable(inner) => type_def_target_ty(ws, bi, ty.with_idx(*inner)),
        // `Coroutine<Y, R, N>` is a compiler builtin with no source declaration.
        // Jump to the YIELD type `Y`'s declaration -- the meaningful navigable
        // type a coroutine value carries. `None` when `Y` is itself a builtin /
        // primitive (e.g. `Coroutine<int>`), since there's nothing to land on.
        hir::TypeExpr::App {
            ctor: hir::TypeCtor::Nominal(name),
            params,
        } if name.as_str() == "Coroutine" => {
            let yield_ty = params.into_iter().next()?;
            type_def_target_ty(ws, bi, ty.with_idx(yield_ty))
        }
        te => type_def_target(ws, te),
    }
}

/// Declaration span for a type annotation's named type. A `Nominal` resolves
/// through `def_of_name`; the `Map` builtin and the `Coroutine` type (which has
/// no struct) fall back to their stdlib landmark fn.
fn type_def_target(ws: &Workspace, te: &hir::TypeExpr) -> Option<Span> {
    match te {
        hir::TypeExpr::App {
            ctor: hir::TypeCtor::Nominal(name),
            ..
        } => def_of_name(ws, *name).or_else(|| builtin_type_landmark(*name, ws)),
        hir::TypeExpr::App {
            ctor: hir::TypeCtor::Map,
            ..
        } => def_of_name(ws, ustr("map_new")),
        // `Vec<T>` / `[T]` both lower to the `Array` ctor; land on the stdlib
        // `Vec` struct.
        hir::TypeExpr::App {
            ctor: hir::TypeCtor::Array,
            ..
        } => def_of_name(ws, ustr("Vec")),
        _ => None,
    }
}

/// Declaration span of a struct field (the field name).
fn struct_field_span(ws: &Workspace, sname: Ustr, field: Ustr) -> Option<Span> {
    struct_def_by_name(ws, sname)?
        .fields
        .iter()
        .find(|f| f.name == field)
        .map(|f| f.span)
}

/// Declaration span of a record-variant payload field.
fn variant_field_span(
    ws: &Workspace,
    enum_name: Option<Ustr>,
    variant: Ustr,
    field: Ustr,
) -> Option<Span> {
    let edef = enum_def_for_variant(ws, enum_name, variant)?;
    let v = edef.variants.iter().find(|v| v.name == variant)?;
    match &v.payload {
        hir::VariantPayload::Record(fields) => {
            fields.iter().find(|f| f.name == field).map(|f| f.span)
        }
        _ => None,
    }
}

/// A name inside a record pattern resolves to the field's *declaration*:
/// `Struct { field: bind }` -> the struct's field decl, `Enum::Variant { field }`
/// -> the variant payload field. Shorthand `{ field }` is excluded (the name is
/// the binding; the node dispatch resolves it). `None` if `offset` isn't on a
/// record-pattern field name.
fn pattern_field_def(ws: &Workspace, body: &Body, offset: usize) -> Option<Span> {
    for pat in body.pattern_arena.values() {
        let hir::Pattern::Record {
            enum_name,
            name,
            fields,
            ..
        } = pat
        else {
            continue;
        };
        for fidx in fields.clone() {
            let f = &body.record_pat_field_arena[fidx];
            let (s, e) = f.name_span.offset_range;
            if s <= offset && offset < e {
                if let hir::Pattern::Bind(b) = &body.pattern_arena[f.pat] {
                    if body
                        .binding_spans
                        .get(*b)
                        .is_some_and(|bs| bs.offset_range == f.name_span.offset_range)
                    {
                        return None; // shorthand -> node dispatch resolves the binding
                    }
                }
                if enum_name.is_none() {
                    if let Some(sp) = name.and_then(|n| struct_field_span(ws, n, f.name)) {
                        return Some(sp);
                    }
                }
                return variant_field_span(ws, *enum_name, (*name)?, f.name);
            }
        }
    }
    None
}

/// Stdlib landmark for a builtin type name with no nominal declaration.
fn builtin_type_landmark(name: Ustr, ws: &Workspace) -> Option<Span> {
    match name.as_str() {
        "Coroutine" => def_of_name(ws, ustr("resume")),
        _ => None,
    }
}

/// Hover content + highlight span at `offset`, formatted to rock's hover
/// conventions:
/// - a value/binding renders `Type: <T>`, where a bare nominal struct/enum is
///   prefixed (`struct Item`, `enum Color`) and a generic instance stays
///   `Box<int>` (matching rock's `to_simple_string`);
/// - a function renders a fenced ```rock fn name(..) -> R``` signature;
/// - a struct-literal head (`Player { .. }`) renders the full `struct Player {
///   .. }` definition.
///
/// Not yet handled (need bone to record the relevant identifier spans, shared
/// with the goto-definition name-span work): a struct/enum *declaration* name,
/// a struct-literal *field* name (vs the head), and pattern field names.
/// Hover tie-break rank: 0 if the node has an informative type, 1 if it has none
/// or resolves to `never`/`<error>`. Used to prefer a concrete-typed node over an
/// equal-span wrapper node (e.g. a lambda body vs its `never`-typed wrapper).
fn node_ty_uninformative(ws: &Workspace, bi: &BodyInference, node: NodeRef) -> u8 {
    let ty = match node {
        NodeRef::Expr(e) => bi.expr_types.get(e).copied(),
        NodeRef::Binding(b) => bi.binding_types.get(b).copied(),
        _ => return 0,
    };
    match ty {
        Some(t) => {
            let te = match t {
                TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
                TyId::Local(idx) => bi.ty_arena[idx],
            };
            u8::from(matches!(
                te,
                hir::TypeExpr::Builtin(hir::BuiltinTy::Never | hir::BuiltinTy::Error)
            ))
        }
        None => 1,
    }
}

pub fn hover_at(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Option<(String, Span)> {
    let module = ws.modules.get(mid.index())?;

    // Body-level nodes: values, calls, struct-literal heads.
    if let Some(owner) = module.body_at(offset) {
        if let (Some(body), Some(bi)) = (module.bodies.get(&owner), inf.per_fn.get(&owner)) {
            // Record-pattern names aren't in `node_index` (a cursor there
            // otherwise falls through to the arm body); resolve them directly.
            if let Some(hit) = hover_record_pat(ws, body, offset) {
                return Some(hit);
            }
            // Prefer the node with an INFORMATIVE type, then the smallest --
            // belt-and-suspenders against small uninformative overlays (e.g. a
            // brace marker or `never`-typed wrapper sharing an offset with a
            // typed node) shadowing the node the user is actually on. A genuine
            // `never` expr with no informative sibling still wins. Offset
            // stability across every identifier char is pinned by
            // bone-query/tests/{completion_sweep,phantom_block_sentinel}.rs.
            if let Some(iv) = body
                .node_index()
                .find(offset, offset + 1)
                .min_by_key(|x| (node_ty_uninformative(ws, bi, x.val), x.stop - x.start))
            {
                match iv.val {
                    NodeRef::Expr(e) => {
                        let span = *body.expr_spans.get(e)?;
                        match &body.expr_arena[e] {
                            hir::Expr::StructLiteral { name, fields } => {
                                // A field name under the cursor -> `field: T
                                // (field of Struct)`; otherwise the head -> the
                                // full struct definition.
                                for fld_idx in fields.clone() {
                                    let fld = &body.struct_lit_field_arena[fld_idx];
                                    let fs = fld.name_span.offset_range.0;
                                    // Cover the name through (but not including) the
                                    // value, so the `:` in `x: 3` hovers as the
                                    // field too. The value has its own expr node;
                                    // the whitespace guard drops the blanks.
                                    let fe = body
                                        .expr_spans
                                        .get(fld.value)
                                        .map_or(fld.name_span.offset_range.1, |s| s.offset_range.0);
                                    if fs <= offset && offset < fe {
                                        if let Some(s) = render_field_of(ws, *name, fld.name) {
                                            return Some((s, fld.name_span));
                                        }
                                    }
                                }
                                if let Some(def) = render_struct_def(ws, *name) {
                                    return Some((def, span));
                                }
                            }
                            hir::Expr::Call { callee: hir::Callee::Unresolved { .. }, .. } => {
                                if let Some((cmid, fid)) = bi.resolved_overload_calls.get(&e) {
                                    let f = &ws.modules.get(cmid.index())?.functions[*fid];
                                    let sig =
                                        render_fn_signature(ws, f.name, f);
                                    return Some((fenced(&sig), span));
                                }
                            }
                            _ => {}
                        }
                        let ty = *bi.expr_types.get(e)?;
                        return Some((format!("Type: {}", render_hover_type(ws, Some(bi), ty)), span));
                    }
                    NodeRef::Binding(b) => {
                        let ty = *bi.binding_types.get(b)?;
                        let span = *body.binding_spans.get(b)?;
                        return Some((format!("Type: {}", render_hover_type(ws, Some(bi), ty)), span));
                    }
                    // A type annotation (`m: Movable`, `-> Vec2`): show the
                    // referenced type's definition, like a struct-literal head.
                    NodeRef::LocalTy(idx) => {
                        let span = *bi.ty_spans.get(idx)?;
                        return Some((render_type_hover(ws, Some(bi), TyId::Local(idx)), span));
                    }
                    NodeRef::StaticTy { mid: ty_mid, idx } => {
                        let span = *ws.modules.get(ty_mid.index())?.ty_spans.get(idx)?;
                        return Some((
                            render_type_hover(ws, Some(bi), TyId::Static(ty_mid, idx)),
                            span,
                        ));
                    }
                    _ => {}
                }
            }
        }
    }

    // Item-level: a declaration name not present in any body.
    for f in module.functions.values() {
        // Skip compiler-generated ops (e.g. a newtype's `__op_add`): their
        // name_span is the newtype/decl name, so they'd shadow the real
        // declaration's hover. Users never hover these.
        if f.name.as_str().starts_with("__") {
            continue;
        }
        let (s, e) = f.name_span.offset_range;
        if s <= offset && offset < e {
            let sig = render_fn_signature(ws, f.name, f);
            return Some((fenced(&sig), f.name_span));
        }
    }
    for s in module.structs.values() {
        let (ss, se) = s.name_span.offset_range;
        if ss <= offset && offset < se {
            return render_struct_def(ws, s.name).map(|d| (d, s.name_span));
        }
        // A field declaration name (`struct S { field: T }` -> hover `field`).
        for fld in &s.fields {
            if in_leading_name(offset, &fld.span, fld.name.as_str().len()) {
                if let Some(t) = render_field_of(ws, s.name, fld.name) {
                    return Some((t, fld.span));
                }
            }
        }
    }
    for en in module.enums.values() {
        let (s, e) = en.name_span.offset_range;
        if s <= offset && offset < e {
            return render_enum_def(ws, en.name).map(|d| (d, en.name_span));
        }
    }
    for nt in module.newtypes.values() {
        let (s, e) = nt.name_span.offset_range;
        if s <= offset && offset < e {
            return Some((
                fenced(&format!("newtype {}({})", nt.name, render_hover_type(ws, None, nt.wrapped))),
                nt.name_span,
            ));
        }
    }
    for g in module.globals.values() {
        let (s, e) = g.name_span.offset_range;
        if s <= offset && offset < e {
            return Some((format!("Type: {}", render_hover_type(ws, None, g.ty)), g.name_span));
        }
        // The type annotation (`global boss: Mob` -> hover `Mob`). A value-less
        // global has no inference body, so the body-level path skips it; resolve
        // the type's recorded span here at item level.
        if let TyId::Static(tmid, idx) = g.ty {
            if let Some(sp) = ws.modules.get(tmid.index()).and_then(|m| m.ty_spans.get(idx).copied()) {
                let (ts, te) = sp.offset_range;
                if ts <= offset && offset < te {
                    return Some((render_type_hover(ws, None, g.ty), sp));
                }
            }
        }
    }

    None
}

fn fenced(body: &str) -> String {
    format!("```rock\n{}\n```", body)
}

/// Hover for a function's SIGNATURE region -- `fn` keyword, name, param parens,
/// `->`, return type -- i.e. anything in the function's span before its body's
/// opening `{`. Needs `src` to locate that brace (no `{` appears in a Rock
/// signature). Body-interior tokens (`let`, `for`, ...) are past the brace and
/// fall through to the normal node resolution. Skips compiler-generated `__`
/// ops (their name_span is the decl name).
pub fn fn_signature_hover(
    ws: &Workspace,
    mid: ModuleId,
    src: &str,
    offset: usize,
) -> Option<(String, Span)> {
    let module = ws.modules.get(mid.index())?;
    for f in module.functions.values() {
        if f.name.as_str().starts_with("__") {
            continue;
        }
        let (s, e) = f.span.offset_range;
        if !(s <= offset && offset < e) {
            continue;
        }
        // Signature ends at the body's opening brace (or the item end for an
        // extern/abstract fn with no body).
        let body_open = src.get(s..e).and_then(|seg| seg.find('{')).map_or(e, |i| s + i);
        if offset < body_open {
            let sig = render_fn_signature(ws, f.name, f);
            return Some((fenced(&sig), f.name_span));
        }
    }
    None
}

/// `enum Name { V1, V2, ... }`, fenced. None if `name` isn't an enum.
fn render_enum_def(ws: &Workspace, name: Ustr) -> Option<String> {
    let edef = enum_def_by_name(ws, name)?;
    let variants: Vec<&str> = edef.variants.iter().map(|v| v.name.as_str()).collect();
    Some(fenced(&format!("enum {} {{ {} }}", name, variants.join(", "))))
}

/// Hover text for a type annotation (`m: Movable`, `-> Vec2`): a bare nominal
/// struct shows its full definition, an enum its variants; anything else
/// (generics, builtins, nullable) falls back to `Type: <rendered>`.
fn render_type_hover(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> String {
    if let Some(hir::TypeExpr::App {
        ctor: hir::TypeCtor::Nominal(name),
        params,
    }) = deref_ty(ws, bi, ty)
    {
        if params.is_empty() {
            let name = *name;
            if let Some(def) = render_struct_def(ws, name) {
                return def;
            }
            if let Some(def) = render_enum_def(ws, name) {
                return def;
            }
        }
    }
    format!("Type: {}", render_hover_type(ws, bi, ty))
}

/// `Type:`-position type rendering: bare nominal struct/enum names are prefixed
/// (`struct Foo` / `enum Bar`); generic instances and builtins fall through to
/// the workspace renderer (`Box<int>`, `int`, `string?`, `[int]`).
fn render_hover_type(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> String {
    if let Some(hir::TypeExpr::App {
        ctor: hir::TypeCtor::Nominal(name),
        params,
    }) = deref_ty(ws, bi, ty)
    {
        if params.is_empty() {
            let name = *name;
            if ws.modules.iter().any(|m| m.struct_by_name.contains_key(&name)) {
                return format!("struct {}", name);
            }
            if ws.modules.iter().any(|m| m.enums_by_name.contains_key(&name)) {
                return format!("enum {}", name);
            }
        }
    }
    ws.render(bi, ty)
}

/// Hover for a name inside a record pattern (`Struct { field: bind }` /
/// `Enum::Variant { field: bind }`): a field name -> `field: T (field of
/// Owner)`; the variant name -> `Type: enum E`. `None` when `offset` isn't on
/// any record-pattern name.
fn hover_record_pat(ws: &Workspace, body: &Body, offset: usize) -> Option<(String, Span)> {
    for pat in body.pattern_arena.values() {
        let hir::Pattern::Record {
            enum_name,
            name,
            name_span,
            fields,
            ..
        } = pat
        else {
            continue;
        };

        // A field name -> `field: T (field of Owner)`. Shorthand `{ field }`
        // is excluded: the name token *is* the binding, so it hovers as a value
        // (`Type: T`) via the node dispatch -- only an explicit `field: bind`
        // (name distinct from its binding) is a field-name reference.
        for fidx in fields.clone() {
            let f = &body.record_pat_field_arena[fidx];
            let (s, e) = f.name_span.offset_range;
            if s <= offset && offset < e {
                if let hir::Pattern::Bind(b) = &body.pattern_arena[f.pat] {
                    if body
                        .binding_spans
                        .get(*b)
                        .is_some_and(|bs| bs.offset_range == f.name_span.offset_range)
                    {
                        return None; // shorthand -> node dispatch renders the binding
                    }
                }
                return render_record_pat_field(ws, *enum_name, *name, f.name)
                    .map(|t| (t, f.name_span));
            }
        }

        // The pattern head name: an enum variant (`E::B`) -> the enum type; a
        // struct head (`Point { .. }`) -> the struct definition.
        if let (Some(nspan), Some(var)) = (name_span, name) {
            let (s, e) = nspan.offset_range;
            if s <= offset && offset < e {
                if let Some(edef) = enum_def_for_variant(ws, *enum_name, *var) {
                    return Some((format!("Type: enum {}", edef.name), *nspan));
                }
                if let Some(def) = render_struct_def(ws, *var) {
                    return Some((def, *nspan));
                }
            }
        }
    }
    None
}

/// `field: T (field of Point)` for a struct pattern, `value: int (field of
/// Opt::Some)` for an enum record-variant pattern. `None` if unresolved.
fn render_record_pat_field(
    ws: &Workspace,
    enum_name: Option<Ustr>,
    name: Option<Ustr>,
    field: Ustr,
) -> Option<String> {
    let name = name?;
    // Unqualified name that is a known struct -> struct field.
    if enum_name.is_none() {
        if let Some(s) = render_field_of(ws, name, field) {
            return Some(s);
        }
    }
    // Enum record variant.
    let ty = variant_record_field_ty(ws, enum_name, Some(name), field)?;
    let edef = enum_def_for_variant(ws, enum_name, name)?;
    Some(format!(
        "{}: {} (field of {}::{})",
        field,
        ws.render(None, ty),
        edef.name,
        name
    ))
}

/// `field: T (field of Struct)` -- rock's hover for a struct field name. `None`
/// if the struct or field isn't found (renders the head's full def instead).
fn render_field_of(ws: &Workspace, struct_name: Ustr, field: Ustr) -> Option<String> {
    let sdef = struct_def_by_name(ws, struct_name)?;
    let f = sdef.fields.iter().find(|f| f.name == field)?;
    Some(format!(
        "{}: {} (field of {})",
        field,
        ws.render(None, f.ty),
        struct_name
    ))
}

/// The full `struct Name { field: T, .. }` definition block (rock's struct-name
/// hover), or `None` if `name` isn't a known struct.
fn render_struct_def(ws: &Workspace, name: Ustr) -> Option<String> {
    let sdef = struct_def_by_name(ws, name)?;
    let mut out = format!("struct {} {{\n", name);
    for f in &sdef.fields {
        out.push_str(&format!("  {}: {},\n", f.name, ws.render(None, f.ty)));
    }
    out.push('}');
    Some(out)
}

fn def_of_name(ws: &Workspace, name: Ustr) -> Option<Span> {
    for m in ws.modules.iter() {
        if let Some(g) = m.global_by_name.get(&name) {
            return Some(m.globals[*g].name_span);
        }

        if let Some(f) = m.functions_by_name.get(&name).and_then(|l| l.first()) {
            return Some(ws.module(f.0).functions[f.1].span);
        }

        if let Some(s) = m.struct_by_name.get(&name) {
            return Some(m.structs[*s].name_span);
        }

        if let Some(en) = m.enums_by_name.get(&name) {
            return Some(m.enums[*en].name_span);
        }

        if let Some(nt) = m.newtypes_by_name.get(&name) {
            return Some(m.newtypes[*nt].span);
        }
    }

    // A bare name that's actually an enum variant (`Red`, or a record-variant
    // literal head `Circle { .. }`) resolves to its enum's name.
    for m in ws.modules.iter() {
        if let Some((idx, _)) = m
            .enums
            .iter()
            .find(|(_, e)| e.variants.iter().any(|v| v.name == name))
        {
            return Some(m.enums[idx].name_span);
        }
    }

    None
}

/// A reference site for find-references/rename. `shorthand` is set when the
/// site is a struct-literal or record-pattern shorthand token (`S { x }`) that
/// stands for both a field name and a same-named binding -- rename must expand
/// the shorthand instead of replacing the token.
#[derive(Clone, Copy, Debug)]
pub struct RefSite {
    pub span: Span,
    pub shorthand: Option<Shorthand>,
}

#[derive(Clone, Copy, Debug)]
pub enum Shorthand {
    /// The renamed symbol is the binding; the field keeps its name:
    /// `{ x }` -> `{ x: <new> }`.
    KeepField(Ustr),
    /// The renamed symbol is the field; the binding keeps its name:
    /// `{ x }` -> `{ <new>: x }`.
    KeepBinding(Ustr),
}

impl RefSite {
    fn plain(span: Span) -> Self {
        RefSite {
            span,
            shorthand: None,
        }
    }
}

pub fn references(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Vec<RefSite> {
    let plain = |spans: Vec<Span>| spans.into_iter().map(RefSite::plain).collect();

    if let Some(local) = local_references(ws, mid, offset) {
        return local;
    }

    if let Some((gmid, gidx)) = generic_param_at(ws, mid, offset) {
        return plain(generic_param_references(ws, gmid, gidx));
    }

    if let Some((ename, variant)) = enum_variant_at(ws, mid, offset) {
        return plain(enum_variant_references(ws, ename, variant));
    }

    if let Some(name) = enum_at(ws, mid, offset) {
        return plain(enum_references(ws, name));
    }

    if let Some(name) = struct_at(ws, mid, inf, offset) {
        // `struct_at` matches any Nominal annotation ctor, including enums
        // (`c: Color`) -- route those to the enum walk so the declaration and
        // `Color::` qualifiers rename too.
        if enum_def_by_name(ws, name).is_some() {
            return plain(enum_references(ws, name));
        }
        return plain(struct_references(ws, name));
    }

    if let Some((sname, field)) = struct_field_at(ws, mid, inf, offset) {
        return field_references(ws, sname, field);
    }

    if let Some((fmid, fid)) = function_target(ws, mid, inf, offset) {
        let name = ws.modules.get(fmid.index()).map(|m| m.functions[fid].name);
        if let Some(name) = name {
            return plain(function_references_by_name(ws, name));
        }
    }

    Vec::new()
}

/// `true` if `offset` falls in `[span.start, span.start + len)` -- the leading
/// `len` bytes of a span (where the name token always sits, for type ctors and
/// struct-literal heads).
fn in_leading_name(offset: usize, span: &Span, len: usize) -> bool {
    let start = span.offset_range.0;
    offset >= start && offset < start + len
}

/// The leading name token of a span (clip to `len`), preserving file/line.
fn leading_name_span(span: &Span, len: usize) -> Span {
    let start = span.offset_range.0;
    Span {
        offset_range: (start, start + len),
        ..*span
    }
}

/// The struct name the cursor is on: a struct *declaration* name, a `Nominal`
/// type annotation's ctor (`p: Point`), or a struct-literal head (`Point { .. }`).
/// `None` otherwise.
fn struct_at(ws: &Workspace, mid: ModuleId, _inf: &InferenceResult, offset: usize) -> Option<Ustr> {
    let module = ws.modules.get(mid.index())?;

    // Declaration name (item-level).
    for s in module.structs.values() {
        if in_leading_name(offset, &s.name_span, s.name.as_str().len()) {
            return Some(s.name);
        }
    }

    // Body node: a type-annotation ctor or a struct-literal head.
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let iv = body
        .node_index()
        .find(offset, offset + 1)
        .min_by_key(|x| x.stop - x.start)?;
    match iv.val {
        NodeRef::StaticTy { mid: tmid, idx } => {
            let m = ws.modules.get(tmid.index())?;
            ctor_name_if_on(&m.ty_arena[idx], m.ty_spans.get(idx), offset)
        }
        NodeRef::LocalTy(idx) => {
            ctor_name_if_on(&body.lowered_tys[idx], body.lowered_ty_spans.get(idx), offset)
        }
        NodeRef::Expr(e) => {
            if let hir::Expr::StructLiteral { name, .. } = &body.expr_arena[e] {
                let sp = body.expr_spans.get(e)?;
                if in_leading_name(offset, sp, name.as_str().len()) {
                    return Some(*name);
                }
            }
            None
        }
        _ => None,
    }
}

/// The nominal ctor name of a type if `offset` is on its leading name token.
fn ctor_name_if_on(te: &hir::TypeExpr, span: Option<&Span>, offset: usize) -> Option<Ustr> {
    if let hir::TypeExpr::App {
        ctor: hir::TypeCtor::Nominal(name),
        ..
    } = te
    {
        if in_leading_name(offset, span?, name.as_str().len()) {
            return Some(*name);
        }
    }
    None
}

/// The `(struct name, field name)` the cursor is on -- a field *declaration*,
/// a `recv.field` access (recv's struct), or a struct-literal field name. Limited
/// to struct fields (enum-variant payload fields aren't covered yet). `None`
/// otherwise.
fn struct_field_at(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Option<(Ustr, Ustr)> {
    let module = ws.modules.get(mid.index())?;

    // Field declaration (`struct S { field: T }`).
    for s in module.structs.values() {
        for f in &s.fields {
            let (fs, fe) = f.span.offset_range;
            if fs <= offset && offset < fe {
                return Some((s.name, f.name));
            }
        }
    }

    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let iv = body
        .node_index()
        .find(offset, offset + 1)
        .min_by_key(|x| x.stop - x.start)?;

    let NodeRef::Expr(e) = iv.val else {
        return None;
    };
    match &body.expr_arena[e] {
        // A field name inside a struct literal.
        hir::Expr::StructLiteral { name, fields } => {
            for fidx in fields.clone() {
                let f = &body.struct_lit_field_arena[fidx];
                let (fs, fe) = f.name_span.offset_range;
                if fs <= offset && offset < fe {
                    return Some((*name, f.name));
                }
            }
            None
        }
        // `recv.field` -- the field is the trailing token of the access span.
        hir::Expr::FieldAccess { base, field } => {
            let sp = body.expr_spans.get(e)?;
            let flen = field.as_str().len();
            if offset + flen < sp.offset_range.1 {
                return None; // cursor is on the receiver, not the field
            }
            let bi = inf.per_fn.get(&owner)?;
            let base_ty = *bi.expr_types.get(*base)?;
            let (sname, _) = struct_app_args(ws, bi, base_ty)?;
            Some((sname, *field))
        }
        _ => None,
    }
}

/// Every reference to struct field `sname.field` across all modules: the field
/// declaration, every `recv.field` access whose receiver is a `sname` (inferred,
/// so a same-named field on a different struct is excluded), every `sname { field:
/// .. }` literal field name, and every `sname { field }` record-pattern field.
fn field_references(ws: &Workspace, sname: Ustr, field: Ustr) -> Vec<RefSite> {
    let flen = field.as_str().len();
    let mut out: Vec<RefSite> = Vec::new();
    let mut push = |site: RefSite| {
        if !out
            .iter()
            .any(|s| s.span.offset_range == site.span.offset_range && s.span.file == site.span.file)
        {
            out.push(site);
        }
    };

    for m in ws.modules.iter() {
        // Declaration.
        if let Some(idx) = m.struct_by_name.get(&sname) {
            if let Some(f) = m.structs[*idx].fields.iter().find(|f| f.name == field) {
                push(RefSite::plain(f.span));
            }
        }

        let module_inf = ws.inference(m.id);
        for (owner, body) in m.bodies.iter() {
            let bi = module_inf.and_then(|inf| inf.per_fn.get(owner));

            for (e, expr) in body.expr_arena.iter() {
                match expr {
                    // `sname { field: .. }` literal field. Shorthand (`{ x }`,
                    // where the value token IS the name token) expands on
                    // rename, keeping the binding: `{ <new>: x }`.
                    hir::Expr::StructLiteral { name, fields } if *name == sname => {
                        for fidx in fields.clone() {
                            let f = &body.struct_lit_field_arena[fidx];
                            if f.name == field {
                                let is_shorthand = body
                                    .expr_spans
                                    .get(f.value)
                                    .is_some_and(|vs| vs.offset_range == f.name_span.offset_range);
                                push(RefSite {
                                    span: f.name_span,
                                    shorthand: is_shorthand
                                        .then_some(Shorthand::KeepBinding(f.name)),
                                });
                            }
                        }
                    }
                    // `recv.field` where recv is a `sname` (type-checked).
                    hir::Expr::FieldAccess { base, field: fname } if *fname == field => {
                        let resolves = bi
                            .and_then(|bi| bi.expr_types.get(*base).copied())
                            .and_then(|t| bi.map(|bi| (bi, t)))
                            .and_then(|(bi, t)| struct_app_args(ws, bi, t))
                            .is_some_and(|(n, _)| n == sname);
                        if resolves {
                            if let Some(sp) = body.expr_spans.get(e) {
                                let end = sp.offset_range.1;
                                push(RefSite::plain(Span {
                                    offset_range: (end - flen, end),
                                    ..*sp
                                }));
                            }
                        }
                    }
                    _ => {}
                }
            }

            // `sname { field }` record-pattern field (struct patterns only).
            // Shorthand (the sub-pattern's binding token IS the name token)
            // expands on rename, keeping the binding.
            for pat in body.pattern_arena.values() {
                if let hir::Pattern::Record {
                    enum_name: None,
                    name: Some(n),
                    fields,
                    ..
                } = pat
                {
                    if *n == sname {
                        for fidx in fields.clone() {
                            let f = &body.record_pat_field_arena[fidx];
                            if f.name == field {
                                let is_shorthand = match body.pattern_arena[f.pat] {
                                    hir::Pattern::Bind(b) => body
                                        .binding_spans
                                        .get(b)
                                        .is_some_and(|bs| {
                                            bs.offset_range == f.name_span.offset_range
                                        }),
                                    _ => false,
                                };
                                push(RefSite {
                                    span: f.name_span,
                                    shorthand: is_shorthand
                                        .then_some(Shorthand::KeepBinding(f.name)),
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    out
}

/// Every reference to struct `name` across all modules: the declaration name,
/// every `Nominal` type-annotation ctor (params/returns/fields in `ty_arena`,
/// `let` annotations in `lowered_tys`), and every struct-literal head. Each site
/// is clipped to just the name token so rename replaces the identifier only.
fn struct_references(ws: &Workspace, name: Ustr) -> Vec<Span> {
    let len = name.as_str().len();
    let mut out: Vec<Span> = Vec::new();
    let mut push = |sp: &Span| {
        let clipped = leading_name_span(sp, len);
        if !out
            .iter()
            .any(|s| s.offset_range == clipped.offset_range && s.file == clipped.file)
        {
            out.push(clipped);
        }
    };

    for m in ws.modules.iter() {
        if let Some(idx) = m.struct_by_name.get(&name) {
            push(&m.structs[*idx].name_span);
        }
        for body in m.bodies.values() {
            for_each_nominal_annotation(ws, m.file, body, name, &mut push);
            // `Point { .. }` literal heads.
            for (e, expr) in body.expr_arena.iter() {
                if matches!(expr, hir::Expr::StructLiteral { name: n, .. } if *n == name) {
                    if let Some(sp) = body.expr_spans.get(e) {
                        push(sp);
                    }
                }
            }
        }
    }

    out
}

/// Call `f` with the span of every type-annotation node in `body` whose
/// nominal ctor is `name`. Walks `node_index` rather than `ty_arena`:
/// `ty_spans` skips bare `Ident`-kind types (lower.rs), so a `p: Point`
/// param/return wouldn't be found by arena iteration -- but every annotation
/// is a `StaticTy`/`LocalTy` node here with its source span.
fn for_each_nominal_annotation(
    ws: &Workspace,
    file: Ustr,
    body: &hir::Body,
    name: Ustr,
    f: &mut impl FnMut(&Span),
) {
    let on_nominal = |te: &hir::TypeExpr| {
        matches!(te, hir::TypeExpr::App { ctor: hir::TypeCtor::Nominal(n), .. } if *n == name)
    };
    for iv in body.node_index().iter() {
        let te = match iv.val {
            NodeRef::StaticTy { mid: tmid, idx } => {
                ws.modules.get(tmid.index()).map(|tm| &tm.ty_arena[idx])
            }
            NodeRef::LocalTy(idx) => Some(&body.lowered_tys[idx]),
            _ => None,
        };
        if te.is_some_and(on_nominal) {
            f(&Span {
                file,
                line_range: (0, 0),
                col_range: (0, 0),
                offset_range: (iv.start, iv.stop),
            });
        }
    }
}

/// `true` if `offset` falls inside `span` (an exact name-token span).
fn span_contains(span: &Span, offset: usize) -> bool {
    let (s, e) = span.offset_range;
    s <= offset && offset < e
}

/// The `(enum name, variant name)` the cursor is on: a variant *declaration*
/// name, a qualified use (`Color::Red`, incl. tuple/record payload heads), or
/// a match-pattern variant name. An unqualified pattern variant resolves to
/// the first enum declaring that variant (structural, like hover). `None`
/// otherwise.
fn enum_variant_at(ws: &Workspace, mid: ModuleId, offset: usize) -> Option<(Ustr, Ustr)> {
    let module = ws.modules.get(mid.index())?;

    for en in module.enums.values() {
        for v in &en.variants {
            if span_contains(&v.name_span, offset) {
                return Some((en.name, v.name));
            }
        }
    }

    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;

    for (_, expr) in body.expr_arena.iter() {
        if let hir::Expr::EnumVariant {
            enum_name,
            variant,
            variant_span,
            ..
        } = expr
        {
            if span_contains(variant_span, offset) {
                return Some((*enum_name, *variant));
            }
        }
    }

    for pat in body.pattern_arena.values() {
        match pat {
            hir::Pattern::EnumVariant {
                enum_name,
                variant,
                variant_span,
                ..
            } if span_contains(variant_span, offset) => {
                let ename = enum_def_for_variant(ws, *enum_name, *variant)?.name;
                return Some((ename, *variant));
            }
            hir::Pattern::Record {
                enum_name: Some(en),
                name: Some(v),
                name_span: Some(nspan),
                ..
            } if span_contains(nspan, offset) => {
                return Some((*en, *v));
            }
            _ => {}
        }
    }

    None
}

/// Every reference to variant `ename::variant` across all modules: the
/// declaration name, every qualified expression use, and every match-pattern
/// site. Same-named variants on other enums are excluded (qualified sites
/// carry the enum name; unqualified patterns resolve structurally).
fn enum_variant_references(ws: &Workspace, ename: Ustr, variant: Ustr) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut push = |sp: Span| {
        if !out
            .iter()
            .any(|s| s.offset_range == sp.offset_range && s.file == sp.file)
        {
            out.push(sp);
        }
    };

    for m in ws.modules.iter() {
        if let Some(idx) = m.enums_by_name.get(&ename) {
            if let Some(v) = m.enums[*idx].variants.iter().find(|v| v.name == variant) {
                push(v.name_span);
            }
        }

        for body in m.bodies.values() {
            for (_, expr) in body.expr_arena.iter() {
                if let hir::Expr::EnumVariant {
                    enum_name,
                    variant: v,
                    variant_span,
                    ..
                } = expr
                {
                    if *enum_name == ename && *v == variant {
                        push(*variant_span);
                    }
                }
            }

            for pat in body.pattern_arena.values() {
                match pat {
                    hir::Pattern::EnumVariant {
                        enum_name,
                        variant: v,
                        variant_span,
                        ..
                    } if *v == variant => {
                        let resolved = enum_def_for_variant(ws, *enum_name, *v).map(|e| e.name);
                        if resolved == Some(ename) {
                            push(*variant_span);
                        }
                    }
                    hir::Pattern::Record {
                        enum_name: Some(en),
                        name: Some(v),
                        name_span: Some(nspan),
                        ..
                    } if *en == ename && *v == variant => {
                        push(*nspan);
                    }
                    _ => {}
                }
            }
        }
    }

    out
}

/// The enum name the cursor is on: an enum *declaration* name or the `Enum::`
/// qualifier of a variant use/pattern. Type-annotation ctors (`c: Color`) are
/// matched by `struct_at` and routed here by `references`.
fn enum_at(ws: &Workspace, mid: ModuleId, offset: usize) -> Option<Ustr> {
    let module = ws.modules.get(mid.index())?;

    for en in module.enums.values() {
        if in_leading_name(offset, &en.name_span, en.name.as_str().len()) {
            return Some(en.name);
        }
    }

    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;

    for (_, expr) in body.expr_arena.iter() {
        if let hir::Expr::EnumVariant {
            enum_name,
            enum_name_span,
            ..
        } = expr
        {
            if span_contains(enum_name_span, offset) {
                return Some(*enum_name);
            }
        }
    }

    for pat in body.pattern_arena.values() {
        let (enum_name, enum_name_span) = match pat {
            hir::Pattern::EnumVariant {
                enum_name: Some(en),
                enum_name_span: Some(sp),
                ..
            } => (*en, sp),
            hir::Pattern::Record {
                enum_name: Some(en),
                enum_name_span: Some(sp),
                ..
            } => (*en, sp),
            _ => continue,
        };
        if span_contains(enum_name_span, offset) {
            return Some(enum_name);
        }
    }

    None
}

/// Every reference to enum `name` across all modules: the declaration name,
/// every `Nominal` type-annotation ctor, and every `name::Variant` qualifier
/// in expressions and patterns. Each site is clipped to the name token.
fn enum_references(ws: &Workspace, name: Ustr) -> Vec<Span> {
    let len = name.as_str().len();
    let mut out: Vec<Span> = Vec::new();
    let mut push = |sp: &Span| {
        let clipped = leading_name_span(sp, len);
        if !out
            .iter()
            .any(|s| s.offset_range == clipped.offset_range && s.file == clipped.file)
        {
            out.push(clipped);
        }
    };

    for m in ws.modules.iter() {
        if let Some(idx) = m.enums_by_name.get(&name) {
            push(&m.enums[*idx].name_span);
        }
        for body in m.bodies.values() {
            for_each_nominal_annotation(ws, m.file, body, name, &mut push);

            for (_, expr) in body.expr_arena.iter() {
                if let hir::Expr::EnumVariant {
                    enum_name,
                    enum_name_span,
                    ..
                } = expr
                {
                    if *enum_name == name {
                        push(enum_name_span);
                    }
                }
            }

            for pat in body.pattern_arena.values() {
                let sp = match pat {
                    hir::Pattern::EnumVariant {
                        enum_name: Some(en),
                        enum_name_span: Some(sp),
                        ..
                    } if *en == name => sp,
                    hir::Pattern::Record {
                        enum_name: Some(en),
                        enum_name_span: Some(sp),
                        ..
                    } if *en == name => sp,
                    _ => continue,
                };
                push(sp);
            }
        }
    }

    out
}

/// The fn generic type param the cursor is on, identified by its shared
/// `ty_arena` slot: every use of `T` in the fn resolves to the single
/// `TypeExpr::Generic` allocated at declaration (`declare_generics`), so slot
/// identity gives exact per-fn scoping -- a same-named `T` on another fn is a
/// different slot. Covers the `<T>` declaration name (via `ty_spans`) and any
/// annotation use (via `node_index`). Struct/enum generics are NOT matched:
/// their payload/field types lower without a body, so use sites aren't
/// enumerable and a decl-only rename would produce broken code.
fn generic_param_at(
    ws: &Workspace,
    mid: ModuleId,
    offset: usize,
) -> Option<(ModuleId, Idx<hir::TypeExpr>)> {
    let module = ws.modules.get(mid.index())?;

    for f in module.functions.values() {
        for g in &f.generics {
            if let TyId::Static(gm, gi) = g {
                let decl = ws.modules.get(gm.index()).and_then(|m| m.ty_spans.get(*gi));
                if decl.is_some_and(|sp| span_contains(sp, offset)) {
                    return Some((*gm, *gi));
                }
            }
        }
    }

    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let iv = body
        .node_index()
        .find(offset, offset + 1)
        .min_by_key(|x| x.stop - x.start)?;
    let NodeRef::StaticTy { mid: tmid, idx } = iv.val else {
        return None;
    };
    let m = ws.modules.get(tmid.index())?;
    match m.ty_arena[idx] {
        hir::TypeExpr::Generic(..) => Some((tmid, idx)),
        // `T?` lowers to one node whose slot is a fresh `Nullable` wrapping
        // the shared generic; the name still leads the span.
        hir::TypeExpr::Nullable(ii)
            if matches!(m.ty_arena[ii], hir::TypeExpr::Generic(..)) =>
        {
            Some((tmid, ii))
        }
        _ => None,
    }
}

/// Every reference to the fn generic at `(gmid, gidx)`: the `<T>` declaration
/// name plus every annotation occurrence (params, return, nested type args,
/// `T?` wrappers), matched by shared-slot identity. Sites are clipped to the
/// leading name token.
fn generic_param_references(ws: &Workspace, gmid: ModuleId, gidx: Idx<hir::TypeExpr>) -> Vec<Span> {
    let Some(m) = ws.modules.get(gmid.index()) else {
        return Vec::new();
    };
    let hir::TypeExpr::Generic(_, gname) = m.ty_arena[gidx] else {
        return Vec::new();
    };
    let len = gname.as_str().len();

    let mut out: Vec<Span> = Vec::new();
    let mut push = |sp: Span| {
        let clipped = leading_name_span(&sp, len);
        if !out
            .iter()
            .any(|s| s.offset_range == clipped.offset_range && s.file == clipped.file)
        {
            out.push(clipped);
        }
    };

    if let Some(decl) = m.ty_spans.get(gidx) {
        push(*decl);
    }

    for body in m.bodies.values() {
        for iv in body.node_index().iter() {
            let NodeRef::StaticTy { mid: tmid, idx } = iv.val else {
                continue;
            };
            if tmid != gmid {
                continue;
            }
            let hits = idx == gidx
                || matches!(m.ty_arena[idx], hir::TypeExpr::Nullable(ii) if ii == gidx);
            if hits {
                push(Span {
                    file: m.file,
                    line_range: (0, 0),
                    col_range: (0, 0),
                    offset_range: (iv.start, iv.stop),
                });
            }
        }
    }

    out
}

fn local_references(ws: &Workspace, mid: ModuleId, offset: usize) -> Option<Vec<RefSite>> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;

    let b = body
        .node_index()
        .find(offset, offset + 1)
        .min_by_key(|x| x.stop - x.start)
        .and_then(|iv| match iv.val {
            NodeRef::Binding(b) => Some(b),
            NodeRef::Expr(e) => match &body.expr_arena[e] {
                hir::Expr::Var(b) => Some(*b),
                hir::Expr::Call {
                    callee: hir::Callee::Local(b),
                    ..
                } => Some(*b),
                _ => None,
            },

            _ => None,
        })?;

    let name = body.binding_arena[b].name;
    let name_len = name.as_str().len();

    // A shorthand token (`S { x }` literal, `S { x } =>` pattern) doubles as
    // the field name: a binding rename must expand it, keeping the field.
    // Shorthand iff a same-named field's name token IS this site's token;
    // explicit `x: x` has the field name on a different token.
    let shorthand_at = |sp: &Span| -> Option<Shorthand> {
        let lit = body
            .struct_lit_field_arena
            .values()
            .any(|f| f.name == name && f.name_span.offset_range == sp.offset_range);
        let pat = body
            .record_pat_field_arena
            .values()
            .any(|f| f.name == name && f.name_span.offset_range == sp.offset_range);
        (lit || pat).then_some(Shorthand::KeepField(name))
    };

    let mut out = Vec::new();

    if let Some(s) = body.binding_spans.get(b) {
        // A param's binding span covers `name: type`; clip to just the name so
        // rename replaces the identifier, not the annotation. (The name always
        // leads the binding span; a plain `let x` is already name-only.)
        let start = s.offset_range.0;
        let span = Span {
            offset_range: (start, start + name_len),
            ..*s
        };
        out.push(RefSite {
            span,
            shorthand: shorthand_at(&span),
        });
    }

    for (e, expr) in body.expr_arena.iter() {
        match expr {
            hir::Expr::Var(x) if *x == b => {
                if let Some(s) = body.expr_spans.get(e) {
                    out.push(RefSite {
                        span: *s,
                        shorthand: shorthand_at(s),
                    });
                }
            }

            hir::Expr::Call {
                callee: hir::Callee::Local(x),
                ..
            } if *x == b => {
                if let Some(s) = body.expr_spans.get(e) {
                    let start = s.offset_range.0;
                    out.push(RefSite::plain(Span {
                        offset_range: (start, start + name_len),
                        ..*s
                    }));
                }
            }

            _ => {}
        }
    }

    Some(out)
}

fn function_target(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Option<(ModuleId, Idx<hir::Function>)> {
    let module = ws.modules.get(mid.index())?;

    if let Some(owner) = module.body_at(offset) {
        if let Some(NodeRef::Expr(e)) = module
            .bodies
            .get(&owner)
            .and_then(|body| {
                body.node_index()
                    .find(offset, offset + 1)
                    .min_by_key(|x| x.stop - x.start)
            })
            .map(|iv| iv.val)
        {
            if let Some((cmid, fid)) = inf
                .per_fn
                .get(&owner)
                .and_then(|bi| bi.resolved_overload_calls.get(&e))
            {
                return Some((*cmid, *fid));
            }
        }
    }

    for (fid, f) in module.functions.iter() {
        // Skip compiler-generated ops (e.g. a newtype's `__op_add`): their
        // name_span IS the newtype/decl name token, so a cursor on the newtype
        // decl would resolve here and `function_references_by_name("__op_add")`
        // would rewrite every `__op_add` across the warm stdlib -- a destructive
        // rename. Newtype decls are handled by their own resolution.
        if f.name.as_str().starts_with("__") {
            continue;
        }
        let (s, e) = f.name_span.offset_range;

        if s <= offset && offset < e {
            return Some((mid, fid));
        }
    }

    None
}

/// Every declaration and call site of a function `name`, across all modules.
/// Name-based (not a single `(mid, fid)`) so renaming touches **all overloads**
/// sharing the name and **every** call that resolves to any of them -- rename
/// of an overloaded or cross-file function is then complete.
fn function_references_by_name(ws: &Workspace, name: Ustr) -> Vec<Span> {
    let mut out = Vec::new();

    // Every overload's declaration name.
    for m in ws.modules.iter() {
        if let Some(fns) = m.functions_by_name.get(&name) {
            for (fmid, fid) in fns.iter() {
                if let Some(fm) = ws.modules.get(fmid.index()) {
                    out.push(fm.functions[*fid].name_span);
                }
            }
        }
    }

    // Every call resolving to a function with this name (any overload).
    for (m, inf) in ws.inferences_iter() {
        let Some(module) = ws.modules.get(m.index()) else {
            continue;
        };

        for (owner, bi) in inf.per_fn.iter() {
            let Some(body) = module.bodies.get(owner) else {
                continue;
            };

            for (call_idx, (cmid, cfid)) in bi.resolved_overload_calls.iter() {
                let resolves_to_name = ws
                    .modules
                    .get(cmid.index())
                    .is_some_and(|cm| cm.functions[*cfid].name == name);
                if resolves_to_name {
                    if let hir::Expr::Call {
                        callee: hir::Callee::Unresolved { name_span, .. },
                        ..
                    } = &body.expr_arena[*call_idx]
                    {
                        out.push(*name_span);
                    }
                }
            }
        }
    }

    out
}

/// `let` bindings without an explicit type annotation whose name-span starts in
/// `[start, end)`, each paired with its rendered inferred type (bone-native
/// text, no `lsp_types`). Drives inlay type hints. Params and already-annotated
/// bindings are skipped -- their type is already visible in the source.
pub fn inferred_let_bindings(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    start: usize,
    end: usize,
) -> Vec<(Span, String)> {
    let mut out = Vec::new();
    let Some(module) = ws.modules.get(mid.index()) else {
        return out;
    };

    for (owner, body) in module.bodies.iter() {
        let Some(bi) = inf.per_fn.get(owner) else {
            continue;
        };

        for (b_idx, binding) in body.binding_arena.iter() {
            if binding.kind != hir::BindingKind::Let || binding.declared_ty.is_some() {
                continue;
            }

            let Some(span) = body.binding_spans.get(b_idx) else {
                continue;
            };
            let s = span.offset_range.0;
            if s < start || s >= end {
                continue;
            }

            let Some(ty) = bi.binding_types.get(b_idx) else {
                continue;
            };
            out.push((*span, ws.render(Some(bi), *ty)));
        }
    }

    out
}

/// Parameter-name hints: `name:` immediately before each positional argument of
/// a resolved call, mapping arg `i` to the callee's declared parameter `i`. The
/// span is the argument's; the facade renders it as a leading hint. Variadic
/// callees are skipped (the arg<->param mapping past the fixed params is
/// ambiguous). Range-filtered to `[start, end)`.
pub fn call_param_hints(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    start: usize,
    end: usize,
) -> Vec<(Span, String)> {
    let mut out = Vec::new();
    let Some(module) = ws.modules.get(mid.index()) else {
        return out;
    };

    for (owner, bi) in inf.per_fn.iter() {
        let Some(body) = module.bodies.get(owner) else {
            continue;
        };

        for (call_idx, (cmid, cfid)) in bi.resolved_overload_calls.iter() {
            let hir::Expr::Call { args, .. } = &body.expr_arena[*call_idx] else {
                continue;
            };
            let Some(f) = ws.modules.get(cmid.index()).map(|m| &m.functions[*cfid]) else {
                continue;
            };
            if f.is_variadic {
                continue;
            }

            for (i, arg) in args.clone().enumerate() {
                let Some(param) = f.param_decls.get(i) else {
                    break;
                };
                let Some(sp) = body.expr_spans.get(arg) else {
                    continue;
                };
                let s = sp.offset_range.0;
                if s >= start && s < end {
                    out.push((*sp, format!("{}:", param.name)));
                }
            }
        }
    }

    out
}

/// In-scope locals + params at `offset`: every param of the enclosing function
/// body, plus `let` bindings whose declaration ends before the cursor. Each
/// paired with its kind and rendered inferred type (bone-native text). Drives
/// local/param completion.
///
/// Approximation: bindings from sibling/nested blocks that have already closed
/// before the cursor are still included (over-completion, never a wrong type).
/// Proper lexical scoping via `scope_arena` is a follow-up.
///
/// Inference may be absent for this body -- completion runs on in-progress code
/// where a single unresolved identifier drops the body from `per_fn`. Bindings
/// still come from lowering (`binding_arena`); types render only when inference
/// is present, else the detail is empty.
pub fn locals_in_scope(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Vec<(Ustr, hir::BindingKind, String, Option<TyId>)> {
    let mut out = Vec::new();
    let Some(module) = ws.modules.get(mid.index()) else {
        return out;
    };
    let Some(owner) = module.body_at(offset) else {
        return out;
    };
    let Some(body) = module.bodies.get(&owner) else {
        return out;
    };
    let bi = inf.per_fn.get(&owner);

    // A `let`-statement binding is in scope only PAST its initializer, not past
    // its name -- `let x = x` must not offer `x` in its own RHS (non-recursive
    // let). Map each let-bound binding to its initializer's end offset; bindings
    // with no entry (for/while-let/if-let/match patterns) keep the name-span
    // fallback below.
    let mut init_end: std::collections::HashMap<Idx<hir::BindingInfo>, usize> = Default::default();
    for (_, stmt) in body.statement_arena.iter() {
        let (leaves, value) = match stmt {
            hir::Statement::Let { name, value } => (vec![*name], *value),
            hir::Statement::LetNewtype { var, value, .. } => (vec![*var], *value),
            hir::Statement::LetTuple { vars, value } => (vars.leaves(), *value),
            _ => continue,
        };
        if let Some(end) = body.expr_spans.get(value).map(|s| s.offset_range.1) {
            for b in leaves {
                init_end.insert(b, end);
            }
        }
    }

    for (b_idx, binding) in body.binding_arena.iter() {
        let in_scope = match binding.kind {
            hir::BindingKind::Param => true,
            hir::BindingKind::Let => match init_end.get(&b_idx) {
                Some(end) => *end <= offset,
                None => body
                    .binding_spans
                    .get(b_idx)
                    .is_some_and(|s| s.offset_range.1 <= offset),
            },
        };
        if !in_scope {
            continue;
        }

        // The TyId rides along with the render: a `TyId::Local` here indexes
        // the cursor body's BodyInference arena -- the same seed an
        // `ExpectedArg` scratch Gamma clones, so it stays resolvable there.
        let ty_id = bi.and_then(|bi| bi.binding_types.get(b_idx).copied());
        let ty = match (bi, ty_id) {
            (Some(bi), Some(t)) => ws.render(Some(bi), t),
            _ => String::new(),
        };
        out.push((binding.name, binding.kind, ty, ty_id));
    }

    out
}

// ---------------------------------------------------------------------------
// member completion: `receiver.<field>` -> the receiver's struct fields
// ---------------------------------------------------------------------------

/// Fields (name + rendered type) of the struct that the dotted receiver path
/// resolves to. `segments` is the receiver before the cursor's `.`
/// (`["line", "start"]` for `line.start.<cursor>`). Resolution is **structural**
/// -- declared param/let/global types, struct-literal initializers, and
/// struct-def field chains -- so it works without inference, which is exactly
/// the state completion fires in (the trigger `.` breaks the body). Empty if the
/// path doesn't resolve to a struct.
pub fn struct_fields_for_path(
    ws: &Workspace,
    mid: ModuleId,
    offset: usize,
    segments: &[String],
) -> Vec<(String, String, Option<TyId>)> {
    // Inference-first for a single-binding receiver: if its recovered inferred
    // type is a generic struct App, render fields with the inferred type args
    // substituted (`p: Pair<int, string>` -> `first: int`, not `first: A`).
    // Falls through to structural resolution when there's no usable inference.
    if let [head] = segments {
        if let Some(fields) = inferred_struct_fields(ws, mid, offset, head) {
            return fields;
        }
    }

    let Some((head, rest)) = segments.split_first() else {
        return Vec::new();
    };
    let Some(mut cur) = receiver_head_struct(ws, mid, offset, head) else {
        return Vec::new();
    };

    for seg in rest {
        let Some(sdef) = struct_def_by_name(ws, cur) else {
            return Vec::new();
        };
        let Some(field) = sdef.fields.iter().find(|f| f.name.as_str() == seg.as_str()) else {
            return Vec::new();
        };
        // Struct field types are module-static -- no body needed.
        let Some(next) = struct_name_of_ty(ws, None, field.ty) else {
            return Vec::new();
        };
        cur = next;
    }

    match struct_def_by_name(ws, cur) {
        Some(sdef) => sdef
            .fields
            .iter()
            .map(|f| (f.name.to_string(), ws.render(None, f.ty), Some(f.ty)))
            .collect(),
        None => Vec::new(),
    }
}

/// Fields of a single-binding receiver via its INFERRED type, substituting the
/// struct's generic params with the inferred App args -- so `p: Pair<int,
/// string>` yields `first: int` / `second: string`, not the declared `A` / `B`.
/// Returns `None` (caller falls back to structural) when the binding has no
/// recovered inference or its type isn't a nominal-struct `App`. Non-generic
/// and nested-generic field types render through unchanged.
fn inferred_struct_fields(
    ws: &Workspace,
    mid: ModuleId,
    offset: usize,
    head: &str,
) -> Option<Vec<(String, String, Option<TyId>)>> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let bi = ws.inference(mid)?.per_fn.get(&owner)?;

    let b_idx = find_binding_by_name(body, head, offset)?;
    let recv_ty = *bi.binding_types.get(b_idx)?;

    let (sname, args) = struct_app_args(ws, bi, recv_ty)?;
    let sdef = struct_def_by_name(ws, sname)?;

    // Positional generic-param names of the struct, so a field whose type is
    // `Generic(_, "A")` maps to the inferred arg at that position.
    let gnames: Vec<Ustr> = sdef
        .generics
        .iter()
        .filter_map(|g| match deref_ty(ws, None, *g)? {
            hir::TypeExpr::Generic(_, n) => Some(*n),
            _ => None,
        })
        .collect();

    Some(
        sdef.fields
            .iter()
            .map(|f| {
                // Generic field -> the receiver's inferred type arg (a TyId in
                // `bi`, the same arena a query-time ExpectedArg is seeded from);
                // else the declared static field type.
                let (rendered, ty_id) = match deref_ty(ws, None, f.ty) {
                    Some(hir::TypeExpr::Generic(_, gname)) => gnames
                        .iter()
                        .position(|n| n == gname)
                        .and_then(|i| args.get(i))
                        .map(|arg| (ws.render(Some(bi), *arg), Some(*arg)))
                        .unwrap_or_else(|| (ws.render(None, f.ty), Some(f.ty))),
                    _ => (ws.render(None, f.ty), Some(f.ty)),
                };
                (f.name.to_string(), rendered, ty_id)
            })
            .collect(),
    )
}

/// `(struct name, inferred type args)` if `ty` is a nominal-struct `App`
/// (peeling a `Nullable`). Args are `TyId`s in the same arena as `ty`.
fn struct_app_args(ws: &Workspace, bi: &BodyInference, ty: TyId) -> Option<(Ustr, Vec<TyId>)> {
    match deref_ty(ws, Some(bi), ty)? {
        hir::TypeExpr::App {
            ctor: hir::TypeCtor::Nominal(name),
            params,
        } => {
            let name = *name;
            let args = params.into_iter().map(|idx| ty.with_idx(idx)).collect();
            Some((name, args))
        }
        hir::TypeExpr::Nullable(inner) => struct_app_args(ws, bi, ty.with_idx(*inner)),
        _ => None,
    }
}

/// Resolve a `TyId` to its `TypeExpr`: module arena for `Static`, the body
/// inference's arena for `Local` (where inferred types live).
fn deref_ty<'a>(
    ws: &'a Workspace,
    bi: Option<&'a BodyInference>,
    ty: TyId,
) -> Option<&'a hir::TypeExpr> {
    match ty {
        TyId::Static(m, idx) => Some(&ws.modules.get(m.index())?.ty_arena[idx]),
        TyId::Local(idx) => Some(&bi?.ty_arena[idx]),
    }
}

/// The struct name a receiver-head identifier resolves to: a local/param's
/// declared type, a `let`'s struct-literal initializer, a `for` loop variable's
/// element type, or a global's type.
fn receiver_head_struct(ws: &Workspace, mid: ModuleId, offset: usize, head: &str) -> Option<Ustr> {
    let module = ws.modules.get(mid.index())?;
    if let Some(owner) = module.body_at(offset) {
        if let Some(body) = module.bodies.get(&owner) {
            if let Some(b_idx) = find_binding_by_name(body, head, offset) {
                let binding = &body.binding_arena[b_idx];
                if let Some(ty) = binding.declared_ty {
                    // A `let`'s annotation lowers into `body.lowered_tys`
                    // (`TyId::Local`); a param's into the module arena (`Static`).
                    return struct_name_of_ty(ws, Some(body), ty);
                }
                if let Some(name) = let_init_struct_name(body, b_idx) {
                    return Some(name);
                }
                if let Some(name) = for_loop_var_struct(ws, body, b_idx) {
                    return Some(name);
                }
                if let Some(ty) = match_pattern_binding_ty(ws, body, b_idx) {
                    return struct_name_of_ty(ws, Some(body), ty);
                }
            }
        }
    }

    // Globals share ONE namespace across files, so a global receiver may be
    // declared in another module (`global player: Player` in player.rock, used
    // in sfx.rock). Search the whole workspace, not just the cursor's module --
    // checking the current module first preserves shadowing if a name collides.
    let name = Ustr::from(head);
    if let Some(g) = module.global_by_name.get(&name) {
        return struct_name_of_ty(ws, None, module.globals[*g].ty);
    }
    for m in ws.modules.iter() {
        if let Some(g) = m.global_by_name.get(&name) {
            return struct_name_of_ty(ws, None, m.globals[*g].ty);
        }
    }

    None
}

/// The in-scope binding named `head`: a param, or the most recent `let` whose
/// declaration ends before `offset`.
fn find_binding_by_name(body: &Body, head: &str, offset: usize) -> Option<Idx<hir::BindingInfo>> {
    let mut best: Option<(usize, Idx<hir::BindingInfo>)> = None;
    for (idx, b) in body.binding_arena.iter() {
        if b.name.as_str() != head {
            continue;
        }
        let in_scope = match b.kind {
            hir::BindingKind::Param => true,
            hir::BindingKind::Let => body
                .binding_spans
                .get(idx)
                .is_some_and(|s| s.offset_range.1 <= offset),
        };
        if !in_scope {
            continue;
        }
        let pos = body.binding_spans.get(idx).map_or(0, |s| s.offset_range.0);
        if best.map_or(true, |(bp, _)| pos >= bp) {
            best = Some((pos, idx));
        }
    }
    best.map(|(_, idx)| idx)
}

/// If `b_idx` is bound by `let x = StructName { .. }`, the struct name.
fn let_init_struct_name(body: &Body, b_idx: Idx<hir::BindingInfo>) -> Option<Ustr> {
    for (_, stmt) in body.statement_arena.iter() {
        if let hir::Statement::Let { name, value } = stmt {
            if *name == b_idx {
                if let hir::Expr::StructLiteral { name: sname, .. } = &body.expr_arena[*value] {
                    return Some(*sname);
                }
            }
        }
    }
    None
}

/// If `b_idx` is a `for <var> in <iter>` loop variable, the struct that the
/// iterable's element type names. Resolves the iterable's element structurally
/// (a `Vec<T>`/`[T]` binding) -- no inference.
fn for_loop_var_struct(ws: &Workspace, body: &Body, b_idx: Idx<hir::BindingInfo>) -> Option<Ustr> {
    for (_, stmt) in body.statement_arena.iter() {
        if let hir::Statement::For { vars, iter, .. } = stmt {
            if pattern_binds(vars, b_idx) {
                return iter_element_struct(ws, body, *iter);
            }
        }
    }
    None
}

fn pattern_binds(pat: &hir::BindingPattern, b_idx: Idx<hir::BindingInfo>) -> bool {
    match pat {
        hir::BindingPattern::Leaf(b) => *b == b_idx,
        hir::BindingPattern::Tuple(ps) => ps.iter().any(|p| pattern_binds(p, b_idx)),
    }
}

/// Struct named by the element type of an iterable expression. Today handles a
/// variable whose declared type is an array (`[T]`); other iterables fall back
/// to None.
fn iter_element_struct(ws: &Workspace, body: &Body, iter: Idx<hir::Expr>) -> Option<Ustr> {
    let hir::Expr::Var(b) = &body.expr_arena[iter] else {
        return None;
    };
    let ty = body.binding_arena[*b].declared_ty?;
    let (te, m) = match ty {
        TyId::Static(m, idx) => (&ws.modules.get(m.index())?.ty_arena[idx], Some(m)),
        TyId::Local(idx) => (&body.lowered_tys[idx], None),
    };
    let hir::TypeExpr::App {
        ctor: hir::TypeCtor::Array,
        params,
    } = te
    else {
        return None;
    };
    let elem_idx = params.start();
    let elem_ty = match m {
        Some(m) => TyId::Static(m, elem_idx),
        None => TyId::Local(elem_idx),
    };
    struct_name_of_ty(ws, Some(body), elem_ty)
}

/// If `b_idx` is bound by a `match` arm pattern (`Opt::Some(p)` / record
/// destructure), the type the variant's payload gives it. Structural -- reads
/// the enum def's `VariantPayload`, no inference.
fn match_pattern_binding_ty(ws: &Workspace, body: &Body, b_idx: Idx<hir::BindingInfo>) -> Option<TyId> {
    for (_, arm) in body.match_arm_arena.iter() {
        if let Some(ty) = pattern_binding_ty(ws, body, arm.pattern, b_idx) {
            return Some(ty);
        }
    }
    None
}

fn pattern_binding_ty(
    ws: &Workspace,
    body: &Body,
    pat: Idx<hir::Pattern>,
    b_idx: Idx<hir::BindingInfo>,
) -> Option<TyId> {
    match &body.pattern_arena[pat] {
        hir::Pattern::EnumVariant {
            enum_name,
            variant,
            sub,
            ..
        } => {
            for (i, sp) in (*sub).into_iter().enumerate() {
                if matches!(&body.pattern_arena[sp], hir::Pattern::Bind(b) if *b == b_idx) {
                    return variant_tuple_field_ty(ws, *enum_name, *variant, i);
                }
                if let Some(t) = pattern_binding_ty(ws, body, sp, b_idx) {
                    return Some(t);
                }
            }
            None
        }
        hir::Pattern::Record {
            enum_name,
            name,
            fields,
            ..
        } => {
            for rf in (*fields).into_iter() {
                let rpf = &body.record_pat_field_arena[rf];
                if matches!(&body.pattern_arena[rpf.pat], hir::Pattern::Bind(b) if *b == b_idx) {
                    return variant_record_field_ty(ws, *enum_name, *name, rpf.name);
                }
                if let Some(t) = pattern_binding_ty(ws, body, rpf.pat, b_idx) {
                    return Some(t);
                }
            }
            None
        }
        hir::Pattern::Tuple { elements } => (*elements)
            .into_iter()
            .find_map(|e| pattern_binding_ty(ws, body, e, b_idx)),
        hir::Pattern::Or { alternatives } => (*alternatives)
            .into_iter()
            .find_map(|a| pattern_binding_ty(ws, body, a, b_idx)),
        _ => None,
    }
}

/// Enum definition for `variant`, by `enum_name` if given, else the first enum
/// across all modules that declares a variant of that name.
/// Hover for an enum-variant pattern (`MobKind::Skeleton`) read from the raw
/// parser AST. bone's HIR `Pattern::EnumVariant` carries no span, but the parser
/// keeps the variant- and enum-name `Ident` spans -- so the facade resolves the
/// pattern here when `hover_at` declines (the match expr is no longer a catch-all
/// for arm patterns). Returns `Type: enum E` and the name span the cursor is on,
/// walking match-arm patterns (recursing tuple/or/variant-arg sub-patterns).
pub fn enum_variant_pattern_hover(
    ws: &Workspace,
    ast: &skull_parser::Ast,
    offset: usize,
) -> Option<(String, Span)> {
    for ed in ast.exprs.values() {
        if let skull_parser::ExprKind::Match { arms, .. } = &ed.kind {
            for arm in arms {
                if let Some(hit) = pat_variant_hover(ws, &arm.pattern, offset) {
                    return Some(hit);
                }
            }
        }
    }
    None
}

fn pat_variant_hover(
    ws: &Workspace,
    pat: &skull_parser::Pattern,
    offset: usize,
) -> Option<(String, Span)> {
    use skull_parser::Pattern;
    let on = |sp: Span| {
        let (s, e) = sp.offset_range;
        s <= offset && offset < e
    };
    match pat {
        Pattern::EnumVariant { enum_name, variant_name, args } => {
            // Cover the whole `enum_name :: variant_name` range so the `::`
            // resolves to the enum too (not the enclosing match expr).
            let full = match enum_name {
                Some(en) => Span {
                    offset_range: (en.span.offset_range.0, variant_name.span.offset_range.1),
                    ..variant_name.span
                },
                None => variant_name.span,
            };
            if on(full) {
                let want = enum_name.as_ref().map(|i| i.name);
                if let Some(edef) = enum_def_for_variant(ws, want, variant_name.name) {
                    return Some((format!("Type: enum {}", edef.name), full));
                }
            }
            args.as_ref()
                .and_then(|ps| ps.iter().find_map(|sub| pat_variant_hover(ws, sub, offset)))
        }
        Pattern::Tuple(ps) | Pattern::Or(ps) => {
            ps.iter().find_map(|sub| pat_variant_hover(ws, sub, offset))
        }
        _ => None,
    }
}

fn enum_def_for_variant(ws: &Workspace, enum_name: Option<Ustr>, variant: Ustr) -> Option<&hir::EnumDef> {
    for m in ws.modules.iter() {
        if let Some(name) = enum_name {
            if let Some(idx) = m.enums_by_name.get(&name) {
                return Some(&m.enums[*idx]);
            }
        } else if let Some((_, e)) = m
            .enums
            .iter()
            .find(|(_, e)| e.variants.iter().any(|v| v.name == variant))
        {
            return Some(e);
        }
    }
    None
}

/// Type of a tuple-variant payload slot (`Opt::Some(Point)` -> slot 0 = Point).
fn variant_tuple_field_ty(ws: &Workspace, enum_name: Option<Ustr>, variant: Ustr, pos: usize) -> Option<TyId> {
    let edef = enum_def_for_variant(ws, enum_name, variant)?;
    let v = edef.variants.iter().find(|v| v.name == variant)?;
    match &v.payload {
        hir::VariantPayload::Tuple(tys) => tys.get(pos).copied(),
        _ => None,
    }
}

/// Type of a named field in a record-variant payload.
fn variant_record_field_ty(ws: &Workspace, enum_name: Option<Ustr>, variant: Option<Ustr>, field: Ustr) -> Option<TyId> {
    let variant = variant?;
    let edef = enum_def_for_variant(ws, enum_name, variant)?;
    let v = edef.variants.iter().find(|v| v.name == variant)?;
    match &v.payload {
        hir::VariantPayload::Record(fields) => {
            fields.iter().find(|f| f.name == field).map(|f| f.ty)
        }
        _ => None,
    }
}

/// Peel a type down to its nominal struct name (through `Nullable`). `body` is
/// needed for `TyId::Local` (a `let`'s annotation lives in `body.lowered_tys`);
/// module-static types (params, fields, globals) pass `None`.
fn struct_name_of_ty(ws: &Workspace, body: Option<&Body>, ty: TyId) -> Option<Ustr> {
    let te = match ty {
        TyId::Static(m, idx) => &ws.modules.get(m.index())?.ty_arena[idx],
        TyId::Local(idx) => &body?.lowered_tys[idx],
    };
    match te {
        hir::TypeExpr::App {
            ctor: hir::TypeCtor::Nominal(name),
            ..
        } => Some(*name),
        // `inner` indexes the same arena as `ty`, so keep its TyId variant.
        hir::TypeExpr::Nullable(inner) => {
            let inner_ty = match ty {
                TyId::Static(m, _) => TyId::Static(m, *inner),
                TyId::Local(_) => TyId::Local(*inner),
            };
            struct_name_of_ty(ws, body, inner_ty)
        }
        _ => None,
    }
}

/// Look up a struct definition by name across all modules.
fn struct_def_by_name(ws: &Workspace, name: Ustr) -> Option<&hir::StructDef> {
    ws.modules
        .iter()
        .find_map(|m| m.struct_by_name.get(&name).map(|i| &m.structs[*i]))
}

/// The construction shape of an enum variant, used to template the payload at
/// completion time (`Variant` / `Variant($0)` / `Variant { $0 }`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum VariantShape {
    Unit,
    Tuple,
    Record,
}

impl VariantShape {
    pub fn of(payload: &hir::VariantPayload) -> Self {
        match payload {
            hir::VariantPayload::Unit => VariantShape::Unit,
            hir::VariantPayload::Tuple(_) => VariantShape::Tuple,
            hir::VariantPayload::Record(_) => VariantShape::Record,
        }
    }
}

/// Variant names of `enum_name` paired with their construction shape, across
/// all modules. Drives the payload template in `EnumName::<cursor>` completion.
pub fn enum_variants_with_shape(ws: &Workspace, enum_name: &str) -> Vec<(String, VariantShape)> {
    let name = Ustr::from(enum_name);
    for m in ws.modules.iter() {
        if let Some(idx) = m.enums_by_name.get(&name) {
            return m.enums[*idx]
                .variants
                .iter()
                .map(|v| (v.name.to_string(), VariantShape::of(&v.payload)))
                .collect();
        }
    }
    Vec::new()
}

/// Record fields (name + rendered type) of an enum record variant
/// (`Effect::Bandage { duration, heal_amount }`). Drives enum-record-literal
/// field completion.
/// The struct the receiver `segments` resolves to (e.g. `v.inner` -> the struct
/// of `inner`), by the structural chain. None when it can't be resolved or isn't
/// a struct. Drives UFCS method completion (generic type args are irrelevant to
/// method lookup, so the inference-substitution path isn't needed here).
pub fn struct_name_for_path(
    ws: &Workspace,
    mid: ModuleId,
    offset: usize,
    segments: &[String],
) -> Option<Ustr> {
    // Inference-first for a single-binding receiver, mirroring
    // `struct_fields_for_path`: a local with no declared type (inferred from a
    // call / method chain, `m = e.get().unwrap()`) resolves via its inferred
    // type. Without this, fields surfaced (they have their own inference-first
    // path) but UFCS methods did not.
    if let [head] = segments {
        if let Some(name) = inferred_struct_name(ws, mid, offset, head) {
            return Some(name);
        }
    }
    let (head, rest) = segments.split_first()?;
    let mut cur = receiver_head_struct(ws, mid, offset, head)?;
    for seg in rest {
        let sdef = struct_def_by_name(ws, cur)?;
        let field = sdef.fields.iter().find(|f| f.name.as_str() == seg.as_str())?;
        cur = struct_name_of_ty(ws, None, field.ty)?;
    }
    Some(cur)
}

/// The struct NAME of a single-binding receiver via its recovered INFERRED type
/// (the name half of [`inferred_struct_fields`]) -- resolves a local with no
/// declared type (`m = make_mob()` / `e.get().unwrap()`).
fn inferred_struct_name(ws: &Workspace, mid: ModuleId, offset: usize, head: &str) -> Option<Ustr> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let bi = ws.inference(mid)?.per_fn.get(&owner)?;
    let b_idx = find_binding_by_name(body, head, offset)?;
    let recv_ty = *bi.binding_types.get(b_idx)?;
    let (sname, _args) = struct_app_args(ws, bi, recv_ty)?;
    Some(sname)
}

/// A UFCS method offered on a receiver: name, rendered signature, and whether
/// any parameter remains AFTER the receiver fills the first one (drives the
/// completion snippet -- `name($0)` when args remain, `name()` when none do).
pub struct UfcsMethod {
    pub name: String,
    pub signature: String,
    pub takes_args: bool,
    /// Rendered return type, or None for a `()`-returning method. Drives
    /// type-directed completion (`recv.method()` as a value of the wanted type).
    pub returns: Option<String>,
    /// The return type as a TyId (module-static), for the typed expected-arg
    /// compare; None when the method returns `()`/nothing.
    pub return_ty: Option<TyId>,
}

/// UFCS methods callable as `recv.method(...)` on a value of struct `name`:
/// every function (in any module) whose FIRST parameter's type is that struct.
/// Skips compiler-generated `__` ops. Mirrors rock, which surfaces these in
/// `recv.` member completion.
pub fn ufcs_methods(ws: &Workspace, name: Ustr) -> Vec<UfcsMethod> {
    let mut out = Vec::new();
    // Overloads collapse to one method item (identical label + insert --
    // the duplicate is popup noise), first-wins to match the general path's
    // per-name fn dedup.
    let mut seen: std::collections::HashSet<Ustr> = std::collections::HashSet::new();
    for m in ws.modules.iter() {
        for f in m.functions.values() {
            if f.name.as_str().starts_with("__") {
                continue;
            }
            if let Some(p0) = f.param_decls.first() {
                if struct_name_of_ty(ws, None, p0.ty) == Some(name) && seen.insert(f.name) {
                    out.push(UfcsMethod {
                        name: f.name.to_string(),
                        signature: render_fn_signature(ws, f.name, f),
                        // the receiver consumes p0; args remain iff there's a p1+
                        takes_args: f.param_decls.len() > 1,
                        returns: f.return_ty.map(|t| ws.render(None, t)),
                        return_ty: f.return_ty,
                    });
                }
            }
        }
    }
    out
}

/// Fields `(name, rendered_type)` of struct `name`, or empty if it isn't a
/// struct. Drives struct-literal field completion (`Name { <field> }`).
pub fn struct_fields_by_name(ws: &Workspace, name: &str) -> Vec<(String, String)> {
    match struct_def_by_name(ws, Ustr::from(name)) {
        Some(sdef) => sdef
            .fields
            .iter()
            .map(|f| (f.name.to_string(), ws.render(None, f.ty)))
            .collect(),
        None => Vec::new(),
    }
}

/// The rendered RETURN type of struct `name`'s field `field`, when that field's
/// type is a function type (`fn(..) -> R` -> `R`); else None. Used to type-direct
/// completion inside a lambda body assigned to a function-typed field.
pub fn struct_field_fn_return_type(ws: &Workspace, name: &str, field: &str) -> Option<String> {
    struct_field_fn_return_ty(ws, name, field).map(|t| ws.render(None, t))
}

/// The declared TyId of struct `name`'s field `field` (Static -- struct defs
/// are lowered module items). Typed twin of `struct_fields_by_name`'s render.
pub fn struct_field_ty(ws: &Workspace, name: &str, field: &str) -> Option<TyId> {
    let sdef = struct_def_by_name(ws, Ustr::from(name))?;
    sdef.fields.iter().find(|f| f.name.as_str() == field).map(|f| f.ty)
}

/// TyId twin of `struct_field_fn_return_type`: the RETURN TyId of a
/// function-typed field.
pub fn struct_field_fn_return_ty(ws: &Workspace, name: &str, field: &str) -> Option<TyId> {
    let sdef = struct_def_by_name(ws, Ustr::from(name))?;
    let f = sdef.fields.iter().find(|f| f.name.as_str() == field)?;
    let te = match f.ty {
        TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
        TyId::Local(_) => return None,
    };
    match te {
        hir::TypeExpr::Function { ret, .. } => Some(f.ty.with_idx(ret)),
        _ => None,
    }
}

pub fn variant_record_fields(ws: &Workspace, enum_name: &str, variant: &str) -> Vec<(String, String)> {
    let ename = Ustr::from(enum_name);
    let vname = Ustr::from(variant);
    for m in ws.modules.iter() {
        if let Some(idx) = m.enums_by_name.get(&ename) {
            if let Some(v) = m.enums[*idx].variants.iter().find(|v| v.name == vname) {
                if let hir::VariantPayload::Record(fields) = &v.payload {
                    return fields
                        .iter()
                        .map(|f| (f.name.to_string(), ws.render(None, f.ty)))
                        .collect();
                }
            }
        }
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// fill match arms (code action: generate missing enum variants)
// ---------------------------------------------------------------------------

/// If `offset` is inside a `match` whose scrutinee is an enum with uncovered
/// variants (and no catch-all arm), return the match expression's span and the
/// generated arm patterns for the missing variants -- `EnumName::Variant => {}`,
/// with payload patterns `(_, _)` (tuple) / `{ .. }` (record). The facade turns
/// these into the edit. None when the cursor isn't in such a match, every
/// variant is already covered, or a `_`/binding arm already makes it exhaustive.
pub fn match_arms_fill(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Option<(Span, Vec<String>)> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let bi = inf.per_fn.get(&owner)?;

    // Smallest `match` expr covering the cursor.
    let mut best: Option<(usize, Idx<hir::Expr>)> = None;
    for (e, expr) in body.expr_arena.iter() {
        if !matches!(expr, hir::Expr::Match { .. }) {
            continue;
        }
        let Some(sp) = body.expr_spans.get(e) else { continue };
        let (s, en) = sp.offset_range;
        if s <= offset && offset < en {
            let len = en - s;
            if best.map_or(true, |(bl, _)| len < bl) {
                best = Some((len, e));
            }
        }
    }
    let (_, e) = best?;
    let (enum_name, missing) = match_missing_variants(ws, body, bi, e)?;
    let arms_text: Vec<String> = missing.iter().map(|v| format_variant_arm(enum_name, v)).collect();
    Some((*body.expr_spans.get(e)?, arms_text))
}

/// The uncovered variants of the `match` at `e`, with its enum, or None when `e`
/// isn't an enum match, is already exhaustive, or a `_`/binding arm makes it so.
/// Drives the fill-match-arms code action. (The non-exhaustive *diagnostic* now
/// comes from bone inference itself -- `bone-inference` `Expr::Match` -- and
/// reaches the LSP via `session.diagnostics()`, so this is the action's only job.)
fn match_missing_variants<'a>(
    ws: &'a Workspace,
    body: &Body,
    bi: &BodyInference,
    e: Idx<hir::Expr>,
) -> Option<(Ustr, Vec<&'a hir::EnumVariantDef>)> {
    let hir::Expr::Match { scrutinee, arms } = &body.expr_arena[e] else {
        return None;
    };

    // Scrutinee's enum (struct_app_args returns the nominal name for any App).
    let scrut_ty = *bi.expr_types.get(*scrutinee)?;
    let (enum_name, _) = struct_app_args(ws, bi, scrut_ty)?;
    let edef = enum_def_by_name(ws, enum_name)?;

    // Covered variants. A GUARDED arm may not fire, so it covers nothing (rock
    // parity: `E::A if c =>` still reports A missing). An unguarded wildcard/
    // binding arm makes the whole match exhaustive.
    let mut covered: std::collections::HashSet<Ustr> = std::collections::HashSet::new();
    for arm_idx in arms.clone() {
        let arm = &body.match_arm_arena[arm_idx];
        if arm.guard.is_some() {
            continue;
        }
        if pattern_is_catch_all(body, arm.pattern) {
            return None;
        }
        collect_covered_variants(body, arm.pattern, &mut covered);
    }

    let missing: Vec<&hir::EnumVariantDef> = edef
        .variants
        .iter()
        .filter(|v| !covered.contains(&v.name))
        .collect();

    if missing.is_empty() {
        return None;
    }
    Some((enum_name, missing))
}

/// If `offset` is inside a struct literal `Name { ... }` that's missing one or
/// more REQUIRED fields (declared on the struct, no default, not already given),
/// return the literal's span and the missing field names in declaration order.
/// None when the cursor isn't in such a literal, the struct is unknown, or every
/// required field is already present. Fields WITH a default are not reported --
/// omitting them is legal, so the literal isn't incomplete without them.
pub fn struct_fields_fill(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Option<(Span, Vec<String>)> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    inf.per_fn.get(&owner)?; // require inference on this body (parity w/ match_arms_fill)

    // Smallest struct literal covering the cursor.
    let mut best: Option<(usize, Idx<hir::Expr>)> = None;
    for (e, expr) in body.expr_arena.iter() {
        if !matches!(expr, hir::Expr::StructLiteral { .. }) {
            continue;
        }
        let Some(sp) = body.expr_spans.get(e) else { continue };
        let (s, en) = sp.offset_range;
        if s <= offset && offset < en {
            let len = en - s;
            if best.map_or(true, |(bl, _)| len < bl) {
                best = Some((len, e));
            }
        }
    }
    let (_, e) = best?;
    let hir::Expr::StructLiteral { name, fields } = &body.expr_arena[e] else {
        return None;
    };

    let sdef = struct_def_by_name(ws, *name)?;

    // Field names already provided in the literal.
    let mut present: std::collections::HashSet<Ustr> = std::collections::HashSet::new();
    for fidx in fields.clone() {
        present.insert(body.struct_lit_field_arena[fidx].name);
    }

    let missing: Vec<String> = sdef
        .fields
        .iter()
        .filter(|f| !f.has_default && !present.contains(&f.name))
        .map(|f| f.name.to_string())
        .collect();

    if missing.is_empty() {
        return None;
    }
    Some((*body.expr_spans.get(e)?, missing))
}

/// What a "Generate function" action needs: the missing call's name, its
/// param specs (name + type), the concrete return type if the call's result is
/// concretely typed (else Unit), and the byte offset to insert the stub at
/// (just past the enclosing function).
pub struct GenerateFn {
    pub name: String,
    pub params: Vec<(String, String)>,
    pub ret: Option<String>,
    pub insert_at: usize,
}

/// If `offset` is on a call `name(...)` whose callee resolves to nothing (no
/// function, local, or value of that name), return the data to generate a stub.
/// None when the cursor isn't on such a call, or when a function of that name
/// DOES exist (then the call is a type/arity error, not a missing function).
pub fn generate_function(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Option<GenerateFn> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let bi = inf.per_fn.get(&owner)?;

    // Smallest call covering the cursor whose callee is still unresolved after
    // inference (an `Unresolved` name not promoted into `resolved_overload_calls`).
    let mut best: Option<(usize, Idx<hir::Expr>)> = None;
    for (e, expr) in body.expr_arena.iter() {
        let hir::Expr::Call { callee: hir::Callee::Unresolved { .. }, .. } = expr else {
            continue;
        };
        if bi.resolved_overload_calls.contains_key(&e) {
            continue;
        }
        let Some(sp) = body.expr_spans.get(e) else { continue };
        let (s, en) = sp.offset_range;
        if s <= offset && offset < en {
            let len = en - s;
            if best.map_or(true, |(bl, _)| len < bl) {
                best = Some((len, e));
            }
        }
    }
    let (_, e) = best?;
    let hir::Expr::Call { callee: hir::Callee::Unresolved { name, .. }, args } =
        &body.expr_arena[e]
    else {
        return None;
    };

    // A function of this name exists somewhere -> the call is an error against
    // it, not a missing function. Don't offer to generate a duplicate.
    if function_name_exists(ws, *name) {
        return None;
    }

    let params: Vec<(String, String)> = args
        .clone()
        .enumerate()
        .map(|(i, arg)| (arg_param_name(body, arg, i), arg_param_type(ws, bi, body, arg)))
        .collect();

    // Return type: a concrete inferred result, else the annotation on the `let`
    // this call feeds (`let s: string = label(7)` -> `-> string`), else Unit.
    // bone doesn't type the result node of an unresolved call, so the use-site
    // annotation is the only signal -- and it's load-bearing: without it the
    // stub returns Unit and the typed `let` would no longer type-check.
    let ret = bi
        .expr_types
        .get(e)
        .copied()
        .and_then(|ty| concrete_type_str(ws, bi, ty))
        .or_else(|| {
            let name = binding_for_value(body, e)?;
            bi.binding_types
                .get(name)
                .copied()
                .and_then(|ty| concrete_type_str(ws, bi, ty))
        });

    let hir::BodyOwner::Function(fid) = owner else {
        return None;
    };
    let insert_at = module.functions[fid].span.offset_range.1;

    Some(GenerateFn {
        name: name.to_string(),
        params,
        ret,
        insert_at,
    })
}

/// Param name for a generated stub: reuse a simple-identifier argument
/// (`tally(count)` -> `count`), else a positional `argN`.
fn arg_param_name(body: &Body, arg: Idx<hir::Expr>, i: usize) -> String {
    match &body.expr_arena[arg] {
        hir::Expr::Var(b) => body.binding_arena[*b].name.to_string(),
        hir::Expr::Global(n) | hir::Expr::SiblingRef(n) => n.to_string(),
        _ => format!("arg{i}"),
    }
}

/// Param type for a generated stub. Prefers a concrete inferred type, but bone
/// doesn't type the argument nodes of an UNRESOLVED call, so it falls back to
/// the literal's type, a `Var`'s binding type, and finally `any` (always a valid
/// param type -- unlike `?`, which is a hard `Infer` error in this position).
fn arg_param_type(ws: &Workspace, bi: &BodyInference, body: &Body, arg: Idx<hir::Expr>) -> String {
    if let Some(s) = bi
        .expr_types
        .get(arg)
        .copied()
        .and_then(|ty| concrete_type_str(ws, bi, ty))
    {
        return s;
    }
    match &body.expr_arena[arg] {
        hir::Expr::Int(_) => "int".to_string(),
        hir::Expr::Float(_) => "float".to_string(),
        hir::Expr::Bool(_) => "bool".to_string(),
        hir::Expr::String(_) | hir::Expr::InterpolatedString(_) => "string".to_string(),
        hir::Expr::Var(b) => bi
            .binding_types
            .get(*b)
            .copied()
            .and_then(|ty| concrete_type_str(ws, bi, ty))
            .unwrap_or_else(|| "any".to_string()),
        _ => "any".to_string(),
    }
}

/// The binding a `let`/`let-newtype` statement assigns `value` to, if `value` is
/// that statement's direct RHS. Lets the generator read the use-site annotation
/// for a call's return type.
fn binding_for_value(body: &Body, value: Idx<hir::Expr>) -> Option<Idx<hir::BindingInfo>> {
    for (_, st) in body.statement_arena.iter() {
        match st {
            hir::Statement::Let { name, value: v } if *v == value => return Some(*name),
            hir::Statement::LetNewtype { var, value: v, .. } if *v == value => return Some(*var),
            _ => {}
        }
    }
    None
}

/// True if any module declares a function named `name`.
fn function_name_exists(ws: &Workspace, name: Ustr) -> bool {
    ws.modules
        .iter()
        .any(|m| m.functions_by_name.get(&name).is_some_and(|l| !l.is_empty()))
}

/// Render `ty` to a user type string, but only if it's a CONCRETE type safe to
/// emit as source -- not an unsolved inference var (`?N`, a bare `?`) or the
/// catch-alls `any`/`unknown`. Generating `-> ?` / `x: ?` is a hard bone error
/// (`unsupported type expr: Infer`), so callers fall back to `any` (params) or
/// omit the return type (Unit) when this returns None.
fn concrete_type_str(ws: &Workspace, bi: &BodyInference, ty: TyId) -> Option<String> {
    let s = ws.render(Some(bi), ty);
    let bad = s.is_empty()
        || s == "any"
        || s == "unknown"
        || s.starts_with('?')
        || s.as_bytes()
            .windows(2)
            .any(|w| w[0] == b'?' && w[1].is_ascii_digit());
    (!bad).then_some(s)
}

fn enum_def_by_name(ws: &Workspace, name: Ustr) -> Option<&hir::EnumDef> {
    ws.modules
        .iter()
        .find_map(|m| m.enums_by_name.get(&name).map(|idx| &m.enums[*idx]))
}

/// A bare `_` / binding pattern (no guard) covers everything left.
fn pattern_is_catch_all(body: &Body, pat: Idx<hir::Pattern>) -> bool {
    matches!(
        body.pattern_arena[pat],
        hir::Pattern::Wildcard | hir::Pattern::Bind(_)
    )
}

fn collect_covered_variants(
    body: &Body,
    pat: Idx<hir::Pattern>,
    out: &mut std::collections::HashSet<Ustr>,
) {
    match &body.pattern_arena[pat] {
        hir::Pattern::EnumVariant { variant, .. } => {
            out.insert(*variant);
        }
        hir::Pattern::Record { name: Some(n), .. } => {
            out.insert(*n);
        }
        hir::Pattern::Or { alternatives } => {
            for alt in alternatives.clone() {
                collect_covered_variants(body, alt, out);
            }
        }
        _ => {}
    }
}

fn format_variant_arm(enum_name: Ustr, v: &hir::EnumVariantDef) -> String {
    let pat = match &v.payload {
        hir::VariantPayload::Unit => format!("{}::{}", enum_name, v.name),
        hir::VariantPayload::Tuple(tys) => {
            format!("{}::{}({})", enum_name, v.name, vec!["_"; tys.len()].join(", "))
        }
        hir::VariantPayload::Record(_) => format!("{}::{} {{ .. }}", enum_name, v.name),
    };
    format!("{pat} => {{}}")
}

// ---------------------------------------------------------------------------
// Feature queries graduated from `backend::gaps` -- completion, symbols,
// signature help, typed holes -- plus the `clean_match`/`fz_boundary`
// matching primitives they and the facade ranking share. Bone-native
// inputs/outputs only; the lsp_types conversion stays in `backend`.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymCat {
    Function,
    Struct,
    Enum,
    Variant,
    Newtype,
    Global,
    Local,
    Param,
    Field,
    Keyword,
}

pub struct CompletionEntry {
    /// Interned candidate name (`Copy`, no heap clone during ranking). The
    /// caller materializes the `String` label only for the post-cap survivors.
    pub name: ustr::Ustr,
    pub kind: SymCat,
    pub detail: Option<String>,
    /// The candidate's type when one is knowable: locals/params carry their
    /// inferred/declared TyId (Locals index the cursor body's inference --
    /// only resolvable against a Gamma seeded from that body), globals their
    /// declared Static TyId. Feeds the typed expected-arg compare
    /// (`ExpectedArg::accepts`); `detail` stays display-only.
    pub ty: Option<TyId>,
    /// Whether a `SymCat::Function` candidate declares any parameters. Drives
    /// the completion snippet: a fn with params inserts `name($0)` (cursor
    /// inside), a param-less fn inserts `name()` (cursor after). Meaningless
    /// for non-function kinds (left `false`).
    pub takes_params: bool,
    /// For a `SymCat::Variant` candidate (qualified `Enum::Variant`), its
    /// construction shape -- drives the payload template (`($0)` / `{ $0 }`).
    /// `None` for every other kind.
    pub variant_shape: Option<VariantShape>,
}

pub struct SymbolEntry {
    pub name: String,
    pub kind: SymCat,
    pub span: Span,
}

pub struct SignatureEntry {
    pub label: String,
    pub params: Vec<String>,
}

// ---------------------------------------------------------------------------
// completion
// ---------------------------------------------------------------------------

/// Completion candidates at `offset`.
///
/// IMPLEMENTED for the **global namespace** by mining the per-module name
/// indexes (`functions_by_name` / `struct_by_name` / `enums_by_name` /
/// `newtype_by_name` / `global_by_name`) across every module in the workspace
/// (user code + warm stdlib + builtin). Functions get a rendered signature as
/// `detail`. The facade filters these by the identifier prefix at the cursor.
///
/// In-scope **locals and params** (from [`locals_in_scope`]) are
/// offered first and shadow same-named globals. NOT yet covered: member/`.`
/// completion (needs the receiver's type + its fields/methods).
///
/// `expr_position` (the cursor is in a value slot, not a type slot) governs enum
/// completion: in an expression slot a user enum is offered as its qualified
/// `Enum::Variant` values (the bare name isn't a usable value); in a type slot
/// the bare `Enum` name is offered instead. Mirrors rock-lsp.
pub fn completion(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
    prefix: &str,
    expr_position: bool,
) -> Vec<CompletionEntry> {
    let mut seen: std::collections::HashSet<ustr::Ustr> = std::collections::HashSet::new();
    let mut out = Vec::new();

    // Filter by the identifier query HERE, before rendering any per-candidate
    // detail (function signatures cost a render each). A post-filter in the
    // caller would render every signature in the workspace and then throw most
    // away. Match is a CLEAN fuzzy match (`clean_match`): the query is a
    // subsequence whose first char is at a word boundary and the rest land at
    // boundaries / in a run -- so `tilecen` finds `tile_center` across the `_`
    // and `log` finds `stone_log`, but a mid-word scatter (`nog` across
    // `__sto(n)e_l(o)(g)`) is dropped rather than shown ranked low. The fuzzy
    // score in the caller (`arg_sort_text`) then ranks tight runs first.
    // `prefix` is already lowercased.
    let pfx = prefix.as_bytes();
    let matches = |name: &str| -> bool { clean_match(name.as_bytes(), pfx) };

    // Locals/params in scope shadow the global namespace -- emit them first and
    // let the shared `seen` set suppress a same-named global.
    for (name, kind, ty, ty_id) in locals_in_scope(ws, mid, inf, offset) {
        if !matches(name.as_str()) {
            continue;
        }
        if seen.insert(name) {
            out.push(CompletionEntry {
                name,
                kind: match kind {
                    BindingKind::Param => SymCat::Param,
                    BindingKind::Let => SymCat::Local,
                },
                detail: (!ty.is_empty()).then_some(ty),
                ty: ty_id,
                takes_params: false,
                variant_shape: None,
            });
        }
    }

    for m in ws.modules.iter() {
        for (name, fns) in m.functions_by_name.iter() {
            if !matches(name.as_str()) {
                continue;
            }
            if !seen.insert(*name) {
                continue;
            }
            let first = fns.first().map(|(fmid, fid)| &ws.modules[fmid.index()].functions[*fid]);
            out.push(CompletionEntry {
                name: *name,
                kind: SymCat::Function,
                // Signature `detail` is deferred to the caller and materialized
                // (from `BoneProject::fn_sigs`) only for the post-cap survivors,
                // not rendered or cloned for every workspace function here.
                detail: None,
                ty: None,
                takes_params: first.is_some_and(|f| !f.param_decls.is_empty()),
                variant_shape: None,
            });
        }
        for name in m.struct_by_name.keys() {
            if matches(name.as_str()) && seen.insert(*name) {
                out.push(CompletionEntry { name: *name, kind: SymCat::Struct, detail: None, ty: None, takes_params: false, variant_shape: None });
            }
        }
        // A user enum in an expression slot is offered as its qualified
        // `Enum::Variant` values (matched by a fuzzy subsequence so typing the
        // variant name -- not the enum -- still surfaces it), and the bare enum
        // is suppressed. In a type slot (or for a stdlib/builtin enum, whose `<..>`
        // file id starts with `<`), the bare `Enum` name is offered as before.
        let user_module = !m.file.as_str().starts_with('<');
        for (name, idx) in m.enums_by_name.iter() {
            if expr_position && user_module {
                for v in m.enums[*idx].variants.iter() {
                    let qualified = format!("{name}::{}", v.name);
                    let qn = ustr::Ustr::from(qualified.as_str());
                    // Clean-match the VARIANT NAME (what the user types, `nog` ->
                    // NotGround -- camelCase-aware) or the qualified form (a
                    // `tarnog` Enum+variant query), dropping mid-word scatter.
                    let vname = v.name.as_str();
                    let hit = pfx.is_empty()
                        || clean_match(vname.as_bytes(), pfx)
                        || clean_match(qualified.as_bytes(), pfx);
                    if hit && seen.insert(qn) {
                        out.push(CompletionEntry {
                            name: qn,
                            kind: SymCat::Variant,
                            detail: Some(name.to_string()),
                            ty: None,
                            takes_params: false,
                            variant_shape: Some(VariantShape::of(&v.payload)),
                        });
                    }
                }
            } else if matches(name.as_str()) && seen.insert(*name) {
                out.push(CompletionEntry { name: *name, kind: SymCat::Enum, detail: None, ty: None, takes_params: false, variant_shape: None });
            }
        }
        for name in m.newtypes_by_name.keys() {
            if matches(name.as_str()) && seen.insert(*name) {
                out.push(CompletionEntry { name: *name, kind: SymCat::Newtype, detail: None, ty: None, takes_params: false, variant_shape: None });
            }
        }
        for (name, gidx) in m.global_by_name.iter() {
            if matches(name.as_str()) && seen.insert(*name) {
                // Carry the rendered type in `detail` -- both for the inline
                // `: type` hint and so arg-position ranking can recognise a
                // global of the expected type (bucket 0).
                let ty = ws.render(None, m.globals[*gidx].ty);
                out.push(CompletionEntry {
                    name: *name,
                    kind: SymCat::Global,
                    detail: (!ty.is_empty()).then_some(ty),
                    ty: Some(m.globals[*gidx].ty),
                    takes_params: false,
                    variant_shape: None,
                });
            }
        }
    }

    // Builtin type names, in a TYPE slot only. `Coroutine`/`Map` are compiler
    // builtins with no entry in the workspace name-index, and the scalar
    // primitives aren't nominal types at all -- neither flows through the loops
    // above. (Stdlib structs like `Vec`/`Entity`/`Arena`/`NativeBuffer*`/`Ptr*`/
    // `Unit` already come through `struct_by_name`.)
    if !expr_position {
        for name in BUILTIN_TYPE_NAMES {
            let n = ustr::Ustr::from(*name);
            if matches(name) && seen.insert(n) {
                out.push(CompletionEntry {
                    name: n,
                    kind: SymCat::Struct,
                    detail: None,
                    ty: None,
                    takes_params: false,
                    variant_shape: None,
                });
            }
        }
    }

    // `yield` / `yield*` keywords, only inside a `co fn` body in an expression
    // slot. Outside a coroutine they're not valid, so don't offer them.
    if expr_position && enclosing_is_coroutine(ws, mid, offset) {
        for kw in ["yield", "yield*"] {
            let n = ustr::Ustr::from(kw);
            if matches(kw) && seen.insert(n) {
                out.push(CompletionEntry {
                    name: n,
                    kind: SymCat::Keyword,
                    detail: None,
                    ty: None,
                    takes_params: false,
                    variant_shape: None,
                });
            }
        }
    }

    out
}

/// Type-name builtins surfaced in a type slot (see the call site). The generic
/// builtins (`Coroutine`, `Map`) and the scalar primitives have no workspace
/// name-index entry.
const BUILTIN_TYPE_NAMES: &[&str] =
    &["Coroutine", "Map", "int", "float", "string", "bool", "any"];

/// Whether the body enclosing `offset` is a `co fn`. Drives `yield`/`yield*`
/// keyword completion (and -- in the facade -- coroutine-fn ranking after a
/// `yield`).
fn enclosing_is_coroutine(ws: &Workspace, mid: ModuleId, offset: usize) -> bool {
    let Some(module) = ws.modules.get(mid.index()) else {
        return false;
    };
    matches!(
        module.body_at(offset),
        Some(hir::BodyOwner::Function(fid)) if module.functions[fid].is_coroutine
    )
}

/// Render `fn name(p: T, ...) -> R` from a HIR function (static types, no body
/// inference context needed).
pub(crate) fn render_fn_signature(ws: &Workspace, name: ustr::Ustr, f: &bone_semantic::hir::Function) -> String {
    let params: Vec<String> = f
        .param_decls
        .iter()
        .map(|p| format!("{}: {}", p.name, ws.render(None, p.ty)))
        .collect();
    let ret = f
        .return_ty
        .map(|t| format!(" -> {}", ws.render(None, t)))
        .unwrap_or_default();
    format!("fn {}({}){}", name, params.join(", "), ret)
}

/// Per-function ctags signature pieces, keyed by declaration-span start:
/// `(params, returns)` where `params` is `"(a: int, b: int)"` and `returns` is
/// the rendered return type (None for a unit/no-return fn). Split form -- not
/// `render_fn_signature`'s combined string -- because ctags carries `signature:`
/// and `returns:` as separate extended fields. Keyed by span start (unique per
/// declaration, so overloads don't collide). Only the on-save ctags path calls
/// this; `document_symbols` stays signature-free so the hot `workspace_symbols`
/// query doesn't render types it won't show.
pub(crate) fn fn_ctags_signatures(
    ws: &Workspace,
    mid: ModuleId,
) -> std::collections::HashMap<usize, (String, Option<String>)> {
    let mut out = std::collections::HashMap::new();
    let Some(m) = ws.modules.get(mid.index()) else {
        return out;
    };
    for (_, f) in m.functions.iter() {
        let params: Vec<String> = f
            .param_decls
            .iter()
            .map(|p| format!("{}: {}", p.name, ws.render(None, p.ty)))
            .collect();
        let returns = f.return_ty.map(|t| ws.render(None, t));
        out.insert(f.span.offset_range.0, (format!("({})", params.join(", ")), returns));
    }
    out
}

// ---------------------------------------------------------------------------
// document symbols
// ---------------------------------------------------------------------------

/// Top-level items declared in `mid` (functions, types, globals) with spans.
///
/// Walks the module's item arenas directly -- no inference needed, every item
/// carries its name + declaration `Span`. The facade maps each `SymbolEntry`
/// to a `DocumentSymbol`.
pub fn document_symbols(ws: &Workspace, mid: ModuleId) -> Vec<SymbolEntry> {
    let Some(m) = ws.modules.get(mid.index()) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut push = |name: ustr::Ustr, kind: SymCat, span: Span| {
        out.push(SymbolEntry { name: name.to_string(), kind, span });
    };

    for (_, f) in m.functions.iter() {
        push(f.name, SymCat::Function, f.span);
    }
    for (_, s) in m.structs.iter() {
        push(s.name, SymCat::Struct, s.span);
    }
    for (_, e) in m.enums.iter() {
        push(e.name, SymCat::Enum, e.span);
    }
    for (_, n) in m.newtypes.iter() {
        push(n.name, SymCat::Newtype, n.span);
    }
    for (_, g) in m.globals.iter() {
        push(g.name, SymCat::Global, g.span);
    }

    out
}

// ---------------------------------------------------------------------------
// signature help
// ---------------------------------------------------------------------------

/// Signature(s) of the call (or struct-literal constructor) enclosing `offset`,
/// plus that expr's source span so the facade can compute the active argument
/// index from cursor position.
///
/// Three resolution tiers, in order of precision:
/// 1. A function call bone resolved to a single overload (`resolved_overload_calls`,
///    same data as goto-definition) -> that one signature.
/// 2. A function call bone could NOT resolve (e.g. a generic call with a lambda
///    arg bone can't disambiguate -> "ambiguous overload") -> every function with
///    the callee's name, the standard LSP overload list. Inference isn't needed.
/// 3. A struct literal `Name { .. }` -> a `Name { f: T, .. }` constructor signature.
///
/// The active-parameter index is intentionally NOT computed here -- it's a
/// cursor/source concern, so the facade fills it from the returned span.
pub fn signature_help(
    ws: &Workspace,
    mid: ModuleId,
    inf: Option<&InferenceResult>,
    offset: usize,
) -> Option<(Vec<SignatureEntry>, Span)> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;

    // Smallest enclosing call/struct-literal expr covering the cursor wins
    // (handles nested calls like `multiply(add(2, 3), 4)`), found through the
    // body's interval index -- not an O(body) arena scan.
    let e = body
        .node_index()
        .find(offset, offset + 1)
        .filter_map(|iv| match iv.val {
            NodeRef::Expr(e) => matches!(
                body.expr_arena[e],
                hir::Expr::Call { .. } | hir::Expr::StructLiteral { .. }
            )
            .then_some((iv.stop - iv.start, e)),
            _ => None,
        })
        .min_by_key(|(len, _)| *len)
        .map(|(_, e)| e)?;
    let call_span = *body.expr_spans.get(e)?;

    match &body.expr_arena[e] {
        hir::Expr::Call { callee, .. } => {
            // Tier 1: the overload bone nailed down.
            if let Some((cmid, fid)) = inf
                .and_then(|inf| inf.per_fn.get(&owner))
                .and_then(|bi| bi.resolved_overload_calls.get(&e))
            {
                let f = &ws.modules.get(cmid.index())?.functions[*fid];
                return Some((vec![fn_sig_entry(ws, f.name, f)], call_span));
            }
            // Tier 2: by-name overload list (resolution was ambiguous/absent).
            let hir::Callee::Unresolved { name, .. } = callee else {
                return None;
            };
            let sigs = functions_named(ws, *name);
            (!sigs.is_empty()).then_some((sigs, call_span))
        }
        hir::Expr::StructLiteral { name, .. } => {
            Some((vec![struct_ctor_sig(ws, *name)?], call_span))
        }
        _ => None,
    }
}

/// Render one function as a `SignatureEntry` (label + per-param labels).
fn fn_sig_entry(ws: &Workspace, name: Ustr, f: &Function) -> SignatureEntry {
    let params = f
        .param_decls
        .iter()
        .map(|p| format!("{}: {}", p.name, ws.render(None, p.ty)))
        .collect();
    SignatureEntry { label: render_fn_signature(ws, name, f), params }
}

/// Every function in the workspace named `name` (user code + stdlib), as the
/// overload list a signature-help popup shows when resolution is ambiguous.
fn functions_named(ws: &Workspace, name: Ustr) -> Vec<SignatureEntry> {
    let mut out = Vec::new();
    for m in ws.modules.iter() {
        if let Some(fns) = m.functions_by_name.get(&name) {
            for (fmid, fid) in fns {
                let f = &ws.modules[fmid.index()].functions[*fid];
                out.push(fn_sig_entry(ws, name, f));
            }
        }
    }
    out
}

/// `Name { f: T, .. }` constructor signature for struct literal `name`, or None.
fn struct_ctor_sig(ws: &Workspace, name: Ustr) -> Option<SignatureEntry> {
    for m in ws.modules.iter() {
        if let Some(sid) = m.struct_by_name.get(&name) {
            let sdef = &m.structs[*sid];
            let params: Vec<String> = sdef
                .fields
                .iter()
                .map(|f| format!("{}: {}", f.name, ws.render(None, f.ty)))
                .collect();
            let label = format!("{} {{ {} }}", name, params.join(", "));
            return Some(SignatureEntry { label, params });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// hole info
// ---------------------------------------------------------------------------

/// Which kind of `?` hole the cursor sits on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HoleKind {
    /// `?` in type position, e.g. `let x: ? = expr`.
    Type,
    /// `?` in value position, e.g. `let x = ?`.
    Expr,
}

/// The typed-hole report: kind, the inferred/expected type (when known), and the
/// bindings in scope at the hole. The type is the hole's solved type -- for a
/// type hole the inference var the `?` lowered to, for an expression hole the
/// fresh var coerced to its surrounding expected type (`int` in `let x: int = ?`,
/// an unsolved `?H` when nothing constrains it). `None` only if inference left
/// the node typeless.
pub struct HoleInfo {
    pub kind: HoleKind,
    pub ty: Option<String>,
    pub in_scope: Vec<(String, String)>,
    /// Source span of the `?` itself, so the hover handler can range its result.
    pub span: Span,
}

/// Hole-info at `offset`, or None when the cursor isn't on a `?` hole. The hole
/// is detected on the raw (pre-lowering) AST (where both kinds survive with a
/// span); the type comes from the lowered node at the cursor -- a type
/// annotation for type holes, an `Expr` for expression holes -- rendered through
/// the body's substitution chain.
pub fn hole_info(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    ast: &Ast,
    offset: usize,
) -> Option<HoleInfo> {
    let (kind, span) = hole_kind_at(ast, offset)?;
    let ty = type_at_offset(ws, mid, inf, offset);
    let in_scope = locals_in_scope(ws, mid, inf, offset)
        .into_iter()
        .map(|(name, _kind, ty, _ty_id)| (name.to_string(), ty))
        .collect();
    Some(HoleInfo { kind, ty, in_scope, span })
}

/// Every `?` hole in `ast`, each with its kind, solved/expected type (when
/// inference could resolve it), and source span. Powers the unfilled-hole
/// diagnostics: expression holes warn ("fill me in"), resolved type holes hint
/// ("inferred as T"). Same detection as `hole_kind_at`, swept over the whole
/// module instead of one cursor offset.
pub fn all_holes(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    ast: &Ast,
) -> Vec<(Span, HoleKind, Option<String>)> {
    let mut out = Vec::new();
    for (_, t) in ast.types.iter() {
        if matches!(t.kind, skull_parser::TypeExprKind::Hole(..)) {
            let ty = type_at_offset(ws, mid, inf, t.span.offset_range.0);
            out.push((t.span, HoleKind::Type, ty));
        }
    }
    for (_, e) in ast.exprs.iter() {
        if matches!(e.kind, skull_parser::ExprKind::Hole(..)) {
            let ty = type_at_offset(ws, mid, inf, e.span.offset_range.0);
            out.push((e.span, HoleKind::Expr, ty));
        }
    }
    out
}

/// The kind of `?` hole whose span covers `offset` plus that span, if any. Type
/// position wins (a `?` is only ever one or the other at a given span).
fn hole_kind_at(ast: &Ast, offset: usize) -> Option<(HoleKind, Span)> {
    let covers = |range: (usize, usize)| range.0 <= offset && offset < range.1;
    for (_, t) in ast.types.iter() {
        if matches!(t.kind, skull_parser::TypeExprKind::Hole(..)) && covers(t.span.offset_range) {
            return Some((HoleKind::Type, t.span));
        }
    }
    for (_, e) in ast.exprs.iter() {
        if matches!(e.kind, skull_parser::ExprKind::Hole(..)) && covers(e.span.offset_range) {
            return Some((HoleKind::Expr, e.span));
        }
    }
    None
}

/// Render the lowered node at `offset`, resolved through the body's substitution
/// chain. A type hole's `?` is a type annotation (`LocalTy`/`StaticTy` -> the
/// inference var); an expression hole's `?` is an `Expr` (its `expr_types`
/// entry, the fresh var coerced from context).
fn type_at_offset(ws: &Workspace, mid: ModuleId, inf: &InferenceResult, offset: usize) -> Option<String> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let bi = inf.per_fn.get(&owner)?;
    let iv = body
        .node_index()
        .find(offset, offset + 1)
        .min_by_key(|x| x.stop - x.start)?;
    let ty = match iv.val {
        NodeRef::LocalTy(idx) => TyId::Local(idx),
        NodeRef::StaticTy { mid, idx } => TyId::Static(mid, idx),
        NodeRef::Expr(e) => *bi.expr_types.get(e)?,
        _ => return None,
    };
    Some(ws.render(Some(bi), ty))
}

/// Word-boundary test mirroring `fuzzy_score`: position 0, or preceded by a
/// delimiter. (Note: delimiter-only -- NO camelCase, unlike `clean_match`.)
pub(crate) fn fz_boundary(cand: &[u8], j: usize) -> bool {
    j == 0 || matches!(cand[j - 1], b'_' | b'.' | b'(' | b')' | b':' | b' ')
}

/// Does `query` (already lowercased) match `cand` (ORIGINAL case) CLEANLY -- the
/// FIRST char at a word boundary, and every later char either at a word boundary
/// or immediately after the previous match (a run)? Word boundaries are
/// delimiters (`_ . ( ) : space`) AND camelCase transitions (a lower/digit
/// followed by an upper -- `NotGround`'s `G`), so PascalCase variant/type names
/// match by their inner words. This is how people fuzzy-type (`tc` / `tilecen`
/// -> `tile_center`, `log` -> `stone_log`, `nog` -> `NotGround`), and it drops
/// coincidental mid-word scatter (`nog` across `__sto(n)e_l(o)(g)` or
/// `Bloody(B)anda(g)e`). Filters completion candidates; ranking still uses the
/// finer `fuzzy_score`. Boolean DP over (query x cand); char compare is
/// case-insensitive. Empty = clean.
pub(crate) fn clean_match(cand: &[u8], query: &[u8]) -> bool {
    if query.is_empty() {
        return true;
    }
    let n = cand.len();
    if n < query.len() {
        return false;
    }
    let is_delim = |b: u8| matches!(b, b'_' | b'.' | b'(' | b')' | b':' | b' ');
    let boundary = |j: usize| {
        j == 0
            || is_delim(cand[j - 1])
            || (cand[j].is_ascii_uppercase() && !cand[j - 1].is_ascii_uppercase())
    };
    // `prev[j]` = query[..=i] cleanly matched with query[i] ending at cand[j].
    let mut prev = vec![false; n];
    let mut cur = vec![false; n];
    for (i, &qc) in query.iter().enumerate() {
        let mut any = false;
        let mut prev_before = false; // some prev[k] true for k < j
        for j in 0..n {
            cur[j] = cand[j].to_ascii_lowercase() == qc
                && if i == 0 {
                    boundary(j)
                } else {
                    // consecutive (run) OR a new run starting at a boundary
                    (j > 0 && prev[j - 1]) || (boundary(j) && prev_before)
                };
            any |= cur[j];
            prev_before |= prev[j];
        }
        if !any {
            return false;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev.iter().any(|&b| b)
}
