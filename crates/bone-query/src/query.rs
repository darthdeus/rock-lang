use bone_core::modules::{UsePathRef, display_name};
use bone_core::type_equiv::TyRef;
use bone_semantic::hir::{BindingKind, Function};
use bone_semantic::inference;
use bone_semantic::{hir::NodeRef, prelude::*};
use skull_parser::Ast;

use crate::types;

pub(crate) fn source_ident(name: Ustr) -> &'static str {
    name.as_str().rsplit("::").next().unwrap_or(name.as_str())
}

// Keep feature-specific tie breakers: one shared winner would change
// navigation when a wrapper overlaps a name.
#[derive(Clone, Copy)]
enum NodePolicy {
    Narrowest,
    Hover,
    Definition,
}

struct ResolvedPosition<'a> {
    ws: &'a Workspace,
    mid: ModuleId,
    module: &'a hir::Module,
    offset: usize,
    owner: Option<hir::BodyOwner>,
    body: Option<&'a Body>,
    bi: Option<&'a BodyInference>,
    node: Option<NodeRef>,
    policy: NodePolicy,
}

impl<'a> ResolvedPosition<'a> {
    fn new(
        ws: &'a Workspace,
        mid: ModuleId,
        inf: Option<&'a InferenceResult>,
        offset: usize,
        policy: NodePolicy,
    ) -> Option<Self> {
        let module = ws.modules.get(mid.index())?;
        let owner = module.body_at(offset);
        let body = owner.and_then(|owner| module.bodies.get(&owner));
        let bi = owner.and_then(|owner| inf?.per_fn.get(&owner));
        let node = body.and_then(|body| {
            body.node_index()
                .find(offset, offset + 1)
                .min_by_key(|iv| {
                    let uninformative = || bi.map_or(0, |bi| node_ty_uninformative(ws, bi, iv.val));
                    let (outside_name, uninformative) = match policy {
                        NodePolicy::Narrowest => (false, 0),
                        NodePolicy::Hover => (false, uninformative()),
                        NodePolicy::Definition => (
                            !definition_node_name_contains(body, iv.val, offset),
                            uninformative(),
                        ),
                    };
                    (outside_name, uninformative, iv.stop - iv.start)
                })
                .map(|iv| iv.val)
        });
        Some(Self {
            ws,
            mid,
            module,
            offset,
            owner,
            body,
            bi,
            node,
            policy,
        })
    }

    fn binding(&self) -> Option<Idx<hir::BindingInfo>> {
        binding_of_node(self.body?, self.node?)
    }

    fn literal_field(&self, expr: Idx<hir::Expr>) -> Option<Token> {
        let body = self.body?;
        let (record, fields) = RecordType::literal(&body.expr_arena[expr])?;
        let include_colon =
            matches!(self.policy, NodePolicy::Hover) && matches!(record, RecordType::Struct(_));
        let field = fields
            .map(|id| &body.struct_lit_field_arena[id])
            .find(|field| {
                let (start, mut end) = field.name_span.offset_range();
                if include_colon {
                    if let Some(value) = body.expr_spans.get(field.value) {
                        end = value.offset_range().0;
                    }
                }
                start <= self.offset && self.offset < end
            })?;
        Some(Token {
            target: RefTarget::Field(record, field.name),
            span: field.name_span,
        })
    }

    fn enum_variant_token(&self, expr: Idx<hir::Expr>) -> Option<Token> {
        let body = self.body?;
        let hir::Expr::EnumVariant {
            enum_name,
            enum_name_span,
            variant,
            variant_span,
            ..
        } = body.expr_arena[expr]
        else {
            return None;
        };
        if let Some(field) = self.literal_field(expr) {
            return Some(field);
        }
        let variant_span = body.name_spans[variant_span];
        if span_contains(&variant_span, self.offset) {
            return Some(Token {
                target: RefTarget::EnumVariant(enum_name, variant),
                span: variant_span,
            });
        }
        let enum_span = body.name_spans[enum_name_span];
        let on_qualifier = enum_span.offset_range().0 <= self.offset
            && self.offset < variant_span.offset_range().0;
        on_qualifier.then_some(Token {
            target: RefTarget::Enum(enum_name),
            span: enum_span,
        })
    }

    fn record_pattern_field(&self) -> Option<Token> {
        self.body?
            .pattern_arena
            .iter()
            .find_map(|(id, _)| self.pattern_field(id))
    }

    fn pattern_field(&self, id: Idx<hir::Pattern>) -> Option<Token> {
        let body = self.body?;
        let hir::Pattern::Record { fields, .. } = body.pattern_arena[id] else {
            return None;
        };
        let field = fields
            .map(|id| &body.record_pat_field_arena[id])
            .find(|field| span_contains(&field.name_span, self.offset))?;
        if pattern_field_is_shorthand(body, field) {
            return None;
        }
        let (record, _) = RecordType::pattern(self.ws, body, self.bi, id)?;
        Some(Token {
            target: RefTarget::Field(record, field.name),
            span: field.name_span,
        })
    }
}

pub fn definition_at(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Option<Span> {
    let at = ResolvedPosition::new(ws, mid, Some(inf), offset, NodePolicy::Definition)?;
    let body = at.body?;
    if let Some(span) = at
        .record_pattern_field()
        .and_then(|field| field.target.declaration(ws))
    {
        return Some(span);
    }
    let owner = at.owner?;
    match at.node? {
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

            hir::Expr::Global(name) => def_of_name(ws, *name),

            // In a `FieldRef<Struct, V>` slot a `.field` ref names the struct's field.
            hir::Expr::SiblingRef(name) => inf
                .per_fn
                .get(&owner)
                .and_then(|bi| field_ref_struct(ws, bi, *bi.expr_types.get(e)?))
                .and_then(|sname| {
                    RefTarget::Field(RecordType::Struct(sname), *name).declaration(ws)
                })
                .or_else(|| def_of_name(ws, *name)),

            hir::Expr::StructLiteral { name, .. } => {
                if let Some(field) = at.literal_field(e) {
                    return field.target.declaration(ws);
                }
                RefTarget::Struct(*name).declaration(ws)
            }

            hir::Expr::EnumVariant { enum_name, .. } => at
                .enum_variant_token(e)
                .map_or(RefTarget::Enum(*enum_name), |token| token.target)
                .declaration(ws),

            hir::Expr::FieldAccess { base, field } => {
                let bi = inf.per_fn.get(&owner)?;
                let base_ty = *bi.expr_types.get(*base)?;
                let sname = types::nominal_name(ws, Some(bi), base_ty)?;
                RefTarget::Field(RecordType::Struct(sname), *field).declaration(ws)
            }

            _ => None,
        },

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

fn definition_node_name_contains(body: &hir::Body, node: NodeRef, offset: usize) -> bool {
    match node {
        NodeRef::Binding(binding) => span_contains(&body.binding_arena[binding].span, offset),
        NodeRef::Expr(expr) => {
            let expr_node = &body.expr_arena[expr];
            match expr_node {
                hir::Expr::Var(_) | hir::Expr::Global(_) | hir::Expr::SiblingRef(_) => body
                    .expr_spans
                    .get(expr)
                    .is_some_and(|span| span_contains(span, offset)),
                hir::Expr::Call {
                    callee: hir::Callee::Unresolved { name_span, .. },
                    ..
                } => span_contains(&body.name_spans[*name_span], offset),
                hir::Expr::EnumVariant {
                    enum_name_span,
                    variant_span,
                    ..
                } => {
                    span_contains(&body.name_spans[*enum_name_span], offset)
                        || span_contains(&body.name_spans[*variant_span], offset)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

pub(crate) fn type_definition_at(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Option<Span> {
    let at = ResolvedPosition::new(ws, mid, Some(inf), offset, NodePolicy::Narrowest)?;
    let bi = at.bi?;
    let ty = match at.node? {
        NodeRef::Binding(b) => *bi.binding_types.get(b)?,
        NodeRef::Expr(e) => *bi.expr_types.get(e)?,
        _ => return None,
    };

    type_def_target_ty(ws, bi, ty)
}

fn type_def_target_ty(ws: &Workspace, bi: &BodyInference, ty: TyId) -> Option<Span> {
    let (ty, te) = ws.resolve_ty(Some(bi), ty)?;
    match te {
        hir::TypeExpr::Nullable(inner) => type_def_target_ty(ws, bi, ty.with_idx(inner)),
        // `Coroutine<Y, ..>` has no declaration; jump to its yield type `Y`.
        hir::TypeExpr::App {
            ctor: hir::TypeCtor::Nominal(name),
            params,
        } if is_intrinsic_key(name, "Coroutine") => {
            let yield_ty = params.into_iter().next()?;
            type_def_target_ty(ws, bi, ty.with_idx(yield_ty))
        }
        te => type_def_target(ws, &te),
    }
}

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
        // `Vec<T>` and `[T]` both lower to the `Array` ctor.
        hir::TypeExpr::App {
            ctor: hir::TypeCtor::Array,
            ..
        } => def_of_name(ws, ustr("Vec")),
        _ => None,
    }
}

fn builtin_type_landmark(name: Ustr, ws: &Workspace) -> Option<Span> {
    is_intrinsic_key(name, "Coroutine")
        .then(|| def_of_name(ws, ustr("resume")))
        .flatten()
}

/// Rank nodes with absent, never or error types below informative types.
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
    sources: Option<&SourceMap>,
) -> Option<(String, Span)> {
    let at = ResolvedPosition::new(ws, mid, Some(inf), offset, NodePolicy::Hover)?;
    let module = at.module;
    let frag = crate::modules_scope::fragment(ws, module.file)?;
    if let (Some(body), Some(bi)) = (at.body, at.bi) {
        if let Some(hit) = hover_record_pat(&at, body) {
            return Some(hit);
        }
        if let Some(node) = at.node {
            match node {
                NodeRef::Expr(e) => {
                    let span = *body.expr_spans.get(e)?;
                    match &body.expr_arena[e] {
                        hir::Expr::StructLiteral { name, .. } => {
                            if let Some(hit) = at.literal_field(e).and_then(|field| field.hover(ws))
                            {
                                return Some(hit);
                            }
                            if let Some(def) = RefTarget::Struct(*name).hover(ws) {
                                return Some((def, span));
                            }
                        }
                        hir::Expr::Call {
                            callee: hir::Callee::Unresolved { name, name_span },
                            ..
                        } => {
                            let selected = bi.resolved_overload_calls.get(&e).copied();
                            if let Some(h) =
                                render_call_overloads_hover(ws, *name, selected, frag, sources)
                            {
                                return Some((h, body.name_spans[*name_span]));
                            }
                        }
                        hir::Expr::EnumVariant { .. } => {
                            if let Some(seg) = path_segment_at(ws, mid, offset) {
                                let text = match seg.target {
                                    hir::SegmentTarget::Variant { enum_key, variant } => {
                                        render_variant(ws, Some(enum_key), variant)
                                    }
                                    hir::SegmentTarget::Def(key) => render_enum_def(ws, key),
                                    hir::SegmentTarget::Module { .. } => None,
                                }?;
                                return Some((text, seg.span));
                            }
                            if let Some(token) = at.enum_variant_token(e) {
                                let hit = token.hover(ws);
                                if hit.is_some() || matches!(token.target, RefTarget::Field(..)) {
                                    return hit;
                                }
                            }
                        }
                        // A `.field` ref in a `FieldRef<Struct, V>` slot shows the field,
                        // not the FieldRef wrapper type.
                        hir::Expr::SiblingRef(name) => {
                            if let Some(sname) = field_ref_struct(ws, bi, *bi.expr_types.get(e)?) {
                                if let Some(s) =
                                    RefTarget::Field(RecordType::Struct(sname), *name).hover(ws)
                                {
                                    return Some((s, span));
                                }
                            }
                        }
                        _ => {}
                    }
                    let mut ty = *bi.expr_types.get(e)?;
                    // An assign target is typed `()`; show its binding's type instead.
                    if types::is_unit(ws, Some(bi), ty) {
                        if let Some(bt) = var_binding_ty_at(ws, mid, inf, offset) {
                            ty = bt;
                        }
                    }
                    return Some((
                        format!("Type: {}", render_hover_type(ws, Some(bi), ty)),
                        span,
                    ));
                }
                NodeRef::Binding(b) => {
                    let ty = *bi.binding_types.get(b)?;
                    let span = *body.binding_spans.get(b)?;
                    return Some((
                        format!("Type: {}", render_hover_type(ws, Some(bi), ty)),
                        span,
                    ));
                }
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

    for (fid, f) in module.functions.iter() {
        // Compiler-generated ops (`__op_add`) reuse the decl's name_span and
        // would shadow its hover.
        if source_ident(f.name).starts_with("__") {
            continue;
        }
        if span_contains(&f.name_span, offset) {
            let sig = render_fn_signature_with_defaults(ws, mid, fid, sources);
            return Some((fenced(&sig), f.name_span));
        }
    }
    for s in module.structs.values() {
        if span_contains(&s.name_span, offset) {
            return render_struct_def(ws, s.name).map(|d| (d, s.name_span));
        }
        for fld in &s.fields {
            if in_leading_name(offset, &fld.span, source_ident(fld.name).len()) {
                if let Some(t) = RecordType::Struct(s.name).render_field(ws, fld.name) {
                    return Some((t, fld.span));
                }
            }
        }
    }
    for en in module.enums.values() {
        if span_contains(&en.name_span, offset) {
            return render_enum_def(ws, en.name).map(|d| (d, en.name_span));
        }
        for v in &en.variants {
            if span_contains(&v.name_span, offset) {
                return render_variant(ws, Some(en.name), v.name).map(|d| (d, v.name_span));
            }
        }
    }
    for nt in module.newtypes.values() {
        if span_contains(&nt.name_span, offset) {
            return Some((
                fenced(&format!(
                    "newtype {}({})",
                    display_name(nt.name.as_str()),
                    render_hover_type(ws, None, nt.wrapped)
                )),
                nt.name_span,
            ));
        }
    }
    for g in module.globals.values() {
        if span_contains(&g.name_span, offset) {
            return Some((
                format!("Type: {}", render_hover_type(ws, None, g.ty)),
                g.name_span,
            ));
        }
        // A value-less global has no inference body, so resolve its type here.
        if let TyId::Static(tmid, idx) = g.ty
            && let Some(sp) = ws
                .modules
                .get(tmid.index())
                .and_then(|m| m.ty_spans.get(idx).copied())
            && span_contains(&sp, offset)
        {
            return Some((render_type_hover(ws, None, g.ty), sp));
        }
    }

    None
}

fn fenced(body: &str) -> String {
    format!("```rock\n{}\n```", body)
}

fn render_call_overloads_hover(
    ws: &Workspace,
    name: Ustr,
    selected: Option<(ModuleId, Idx<Function>)>,
    frag: &bone_core::modules::FragmentScope,
    sources: Option<&SourceMap>,
) -> Option<String> {
    let all: Vec<(ModuleId, Idx<Function>)> =
        crate::modules_scope::admitted_overloads(ws, frag, name)
            .map(|(fmid, fid, _)| (fmid, fid))
            .collect();
    if all.is_empty() {
        return None;
    }

    let mut ordered = Vec::with_capacity(all.len());
    if let Some(sel) = selected.filter(|s| all.contains(s)) {
        ordered.push(sel);
        ordered.extend(all.iter().copied().filter(|&fq| fq != sel));
    } else {
        ordered = all;
    }

    let sig = |(m, fid): (ModuleId, Idx<Function>)| {
        render_fn_signature_with_defaults(ws, m, fid, sources)
    };

    if ordered.len() == 1 {
        return Some(fenced(&sig(ordered[0])));
    }

    let others = ordered.len() - 1;
    let mut out = String::new();
    if selected.is_some() {
        out.push_str(&fenced(&sig(ordered[0])));
        out.push_str(&format!(
            "\n\n*+{} other overload{}:*\n",
            others,
            if others == 1 { "" } else { "s" }
        ));
        out.push_str(&fenced(
            &ordered[1..]
                .iter()
                .map(|&fq| sig(fq))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
    } else {
        out.push_str(&format!("*{} overloads:*\n", ordered.len()));
        out.push_str(&fenced(
            &ordered
                .iter()
                .map(|&fq| sig(fq))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
    }
    Some(out)
}

pub(crate) fn fn_signature_hover(
    ws: &Workspace,
    mid: ModuleId,
    src: &str,
    offset: usize,
    sources: Option<&SourceMap>,
) -> Option<(String, Span)> {
    let module = ws.modules.get(mid.index())?;
    for (fid, f) in module.functions.iter() {
        if source_ident(f.name).starts_with("__") {
            continue;
        }
        let (s, e) = f.span.offset_range();
        if !(s <= offset && offset < e) {
            continue;
        }
        // Ends at the body brace, so body keywords are not swallowed.
        let body_open = src
            .get(s..e)
            .and_then(|seg| seg.find('{'))
            .map_or(e, |i| s + i);
        if offset < body_open {
            let sig = render_fn_signature_with_defaults(ws, mid, fid, sources);
            return Some((fenced(&sig), f.name_span));
        }
    }
    None
}

fn render_enum_def(ws: &Workspace, name: Ustr) -> Option<String> {
    let edef = enum_def_by_name(ws, name)?;
    let mut out = format!("enum {} {{\n", display_name(name.as_str()));
    for v in &edef.variants {
        out.push_str(&format!("  {},\n", render_variant_payload(ws, v)));
    }
    out.push('}');
    Some(fenced(&out))
}

fn render_variant_payload(ws: &Workspace, v: &hir::EnumVariantDef) -> String {
    match &v.payload {
        hir::VariantPayload::Unit => v.name.to_string(),
        hir::VariantPayload::Tuple(tys) => {
            let parts: Vec<String> = tys.iter().map(|t| ws.render(None, *t)).collect();
            format!("{}({})", v.name, parts.join(", "))
        }
        hir::VariantPayload::Record(fields) => {
            let parts: Vec<String> = fields
                .iter()
                .map(|f| format!("{}: {}", f.name, ws.render(None, f.ty)))
                .collect();
            format!("{} {{ {} }}", v.name, parts.join(", "))
        }
    }
}

fn render_variant(ws: &Workspace, enum_name: Option<Ustr>, variant: Ustr) -> Option<String> {
    let edef = enum_def_for_variant(ws, enum_name, variant)?;
    let v = edef.variants.iter().find(|v| v.name == variant)?;
    Some(fenced(&format!(
        "{}::{}",
        display_name(edef.name.as_str()),
        render_variant_payload(ws, v)
    )))
}

/// Bare record heads keep the struct-literal HIR shape; a real struct is
/// never treated as a variant.
fn variant_def_span(ws: &Workspace, enum_name: Option<Ustr>, variant: Ustr) -> Option<Span> {
    let edef = enum_def_for_variant(ws, enum_name, variant)?;
    edef.variants
        .iter()
        .find(|v| v.name == variant)
        .map(|v| v.name_span)
}

fn bare_nominal_name(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> Option<Ustr> {
    match ws.resolve_ty(bi, ty)? {
        (
            _,
            hir::TypeExpr::App {
                ctor: hir::TypeCtor::Nominal(name),
                params,
            },
        ) if params.is_empty() => Some(name),
        _ => None,
    }
}

fn render_type_hover(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> String {
    if let Some(name) = bare_nominal_name(ws, bi, ty) {
        if let Some(def) = render_struct_def(ws, name) {
            return def;
        }
        if let Some(def) = render_enum_def(ws, name) {
            return def;
        }
    }
    format!("Type: {}", render_hover_type(ws, bi, ty))
}

fn render_hover_type(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> String {
    if let Some(name) = bare_nominal_name(ws, bi, ty) {
        if ws
            .modules
            .iter()
            .any(|m| m.struct_by_name.contains_key(&name))
        {
            return format!("struct {}", display_name(name.as_str()));
        }
        if ws
            .modules
            .iter()
            .any(|m| m.enums_by_name.contains_key(&name))
        {
            return format!("enum {}", display_name(name.as_str()));
        }
    }
    ws.render(bi, ty)
}

fn hover_record_pat(at: &ResolvedPosition<'_>, body: &Body) -> Option<(String, Span)> {
    let (ws, offset) = (at.ws, at.offset);
    if let Some(field) = at.record_pattern_field() {
        return field.hover(ws);
    }
    for pat in body.pattern_arena.values() {
        let hir::Pattern::Record {
            enum_name,
            name,
            name_span,
            ..
        } = pat
        else {
            continue;
        };
        if let (Some(nspan), Some(var)) = (name_span, name) {
            if span_contains(nspan, offset) {
                if let Some(edef) = enum_def_for_variant(ws, *enum_name, *var) {
                    return Some((
                        format!("Type: enum {}", display_name(edef.name.as_str())),
                        *nspan,
                    ));
                }
                if let Some(def) = render_struct_def(ws, *var) {
                    return Some((def, *nspan));
                }
            }
        }
    }
    None
}

fn render_struct_def(ws: &Workspace, name: Ustr) -> Option<String> {
    let sdef = struct_def_by_name(ws, name)?;
    let mut out = format!("struct {} {{\n", display_name(name.as_str()));
    for f in &sdef.fields {
        out.push_str(&format!("  {}: {},\n", f.name, ws.render(None, f.ty)));
    }
    out.push('}');
    Some(out)
}

pub(crate) fn path_segment_at(
    ws: &Workspace,
    mid: ModuleId,
    offset: usize,
) -> Option<hir::PathSegmentRef> {
    let module = ws.modules.get(mid.index())?;
    let hit = |seg: &&hir::PathSegmentRef| span_contains(&seg.span, offset);
    module
        .body_at(offset)
        .and_then(|owner| module.bodies.get(&owner))
        .and_then(|body| body.path_segments.iter().find(hit))
        .or_else(|| module.path_segments.iter().find(hit))
        .copied()
}

/// `None` for a module target: its headers live in source, not the workspace.
pub(crate) fn segment_target_def(ws: &Workspace, target: hir::SegmentTarget) -> Option<Span> {
    match target {
        hir::SegmentTarget::Def(key) => {
            match ws.functions_by_name.get(&key).and_then(|l| l.first()) {
                Some(&(fmid, fid)) => Some(ws.module(fmid).functions[fid].name_span),
                None => def_of_name(ws, key),
            }
        }
        hir::SegmentTarget::Variant { enum_key, variant } => {
            variant_def_span(ws, Some(enum_key), variant)
        }
        hir::SegmentTarget::Module { .. } => None,
    }
}

pub(crate) fn segments_naming(
    ws: &Workspace,
    target: hir::SegmentTarget,
) -> Vec<hir::PathSegmentRef> {
    let mut out = Vec::new();
    for m in ws.modules.iter() {
        out.extend(m.path_segments.iter().filter(|s| s.target == target));
        for body in m.bodies.values() {
            out.extend(body.path_segments.iter().filter(|s| s.target == target));
        }
    }
    out
}

fn is_source_definition(ws: &Workspace, key: Ustr) -> bool {
    ws.functions_by_name.get(&key).is_none_or(|functions| {
        functions
            .iter()
            .any(|(mid, fid)| !ws.module(*mid).functions[*fid].is_synthetic)
    })
}

pub(crate) fn v1_def_declared_at(ws: &Workspace, decl: &Span) -> Option<Ustr> {
    ws.defs
        .keys()
        .copied()
        .filter(|key| is_source_definition(ws, *key))
        .find(|key| {
            segment_target_def(ws, hir::SegmentTarget::Def(*key)).is_some_and(|d| d == *decl)
        })
}

pub(crate) fn v1_def_named_at(ws: &Workspace, file: Ustr, offset: usize) -> Option<Ustr> {
    ws.defs
        .keys()
        .copied()
        .filter(|key| is_source_definition(ws, *key))
        .find(|key| {
            segment_target_def(ws, hir::SegmentTarget::Def(*key))
                .is_some_and(|d| d.file == file && span_contains(&d, offset))
        })
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
            return Some(m.newtypes[*nt].name_span);
        }
    }

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

/// `shorthand` is set for a shorthand token (`S { x }`) that rename must
/// expand instead of replacing.
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
    pub(crate) fn plain(span: Span) -> Self {
        RefSite {
            span,
            shorthand: None,
        }
    }
}

/// Path -> current file text, for tokens HIR keeps no span for. `None`
/// skips the site rather than mis-spanning it.
pub(crate) type TextOf<'a, 't> = &'a dyn Fn(&str) -> Option<&'t str>;

/// Whether `file` is a compiler-provided `<...>` source no edit may touch.
pub(crate) fn is_read_only_source(file: &str) -> bool {
    rock_stdlib::is_stdlib_module_file(file) || file.starts_with('<')
}

/// Find-usages wants only the overload under the cursor; rename rewrites
/// the whole shared name.
#[derive(Clone, Copy, PartialEq)]
pub enum FnRefScope {
    ThisOverload,
    AllOverloads,
}

#[derive(Clone, Copy)]
enum RefTarget {
    Binding(hir::BodyOwner, Idx<hir::BindingInfo>),
    GenericParam(ModuleId, Idx<hir::TypeExpr>),
    EnumVariant(Ustr, Ustr),
    Enum(Ustr),
    Newtype(Ustr),
    Struct(Ustr),
    Field(RecordType, Ustr),
    Function(ModuleId, Idx<hir::Function>),
    Global(Ustr),
    Decorator(Ustr),
}

impl ResolvedPosition<'_> {
    fn generic(&self) -> Option<(ModuleId, Idx<hir::TypeExpr>)> {
        for ty in declared_generics(self.module) {
            if let TyId::Static(mid, idx) = *ty {
                if self
                    .ws
                    .modules
                    .get(mid.index())
                    .and_then(|module| module.ty_spans.get(idx))
                    .is_some_and(|span| span_contains(span, self.offset))
                {
                    return Some((mid, idx));
                }
            }
        }
        let NodeRef::StaticTy { mid, idx } = self.node? else {
            return None;
        };
        let arena = &self.ws.modules.get(mid.index())?.ty_arena;
        match arena[idx] {
            hir::TypeExpr::Generic(..) => Some((mid, idx)),
            hir::TypeExpr::Nullable(inner)
                if matches!(arena[inner], hir::TypeExpr::Generic(..)) =>
            {
                Some((mid, inner))
            }
            _ => None,
        }
    }

    fn symbol<'t>(&self, text_of: TextOf<'_, 't>) -> Option<RefTarget> {
        let Self {
            ws,
            mid,
            module,
            offset,
            ..
        } = *self;
        if let Some(binding) = self
            .binding()
            .filter(|&b| self.body.is_some_and(|body| body.deco_target != Some(b)))
        {
            return Some(RefTarget::Binding(self.owner?, binding));
        }
        if let Some((mid, idx)) = self.generic() {
            return Some(RefTarget::GenericParam(mid, idx));
        }
        // Equal ranks keep the first occurrence, including ambiguous variants.
        let mut found = None;
        let mut offer = |rank, target| {
            if found.as_ref().is_none_or(|(best, _)| rank < *best) {
                found = Some((rank, target));
            }
        };
        for en in module.enums.values() {
            if in_leading_name(offset, &en.name_span, source_ident(en.name).len()) {
                offer(10, RefTarget::Enum(en.name));
            }
            for variant in &en.variants {
                if span_contains(&variant.name_span, offset) {
                    offer(0, RefTarget::EnumVariant(en.name, variant.name));
                }
                if let hir::VariantPayload::Record(fields) = &variant.payload {
                    for field in fields {
                        if span_contains(&field.span, offset) {
                            offer(
                                50,
                                RefTarget::Field(
                                    RecordType::Variant(en.name, variant.name),
                                    field.name,
                                ),
                            );
                        }
                    }
                }
            }
        }
        for nt in module.newtypes.values() {
            if span_contains(&nt.name_span, offset) {
                offer(20, RefTarget::Newtype(nt.name));
            }
        }
        for record in module.structs.values() {
            if in_leading_name(offset, &record.name_span, source_ident(record.name).len()) {
                offer(30, RefTarget::Struct(record.name));
            }
            for field in &record.fields {
                if span_contains(&field.span, offset) {
                    offer(
                        40,
                        RefTarget::Field(RecordType::Struct(record.name), field.name),
                    );
                }
            }
        }
        for (fid, function) in module.functions.iter() {
            if !source_ident(function.name).starts_with("__")
                && span_contains(&function.name_span, offset)
            {
                offer(61, RefTarget::Function(mid, fid));
            }
            for annotation in &function.annotations {
                let key = annotation.resolved_name.unwrap_or(annotation.name.name);
                if span_contains(&annotation.name.span, offset) && decorator_declared(ws, key) {
                    offer(81, RefTarget::Decorator(key));
                }
            }
        }
        for global in module.globals.values() {
            if span_contains(&global.name_span, offset) {
                offer(70, RefTarget::Global(global.name));
            }
        }
        for decorator in module.decorators.values() {
            if decorator_name_span(decorator, text_of)
                .is_some_and(|span| span_contains(&span, offset))
            {
                offer(80, RefTarget::Decorator(decorator.def_key));
            }
        }
        if let Some(body) = self.body {
            let selected = self.node;
            let nominal = match selected {
                Some(NodeRef::StaticTy { mid, idx }) => {
                    let module = ws.modules.get(mid.index())?;
                    ctor_name_if_on(&module.ty_arena[idx], module.ty_spans.get(idx), offset)
                }
                Some(NodeRef::LocalTy(idx)) => ctor_name_if_on(
                    &body.lowered_tys[idx],
                    body.lowered_ty_spans.get(idx),
                    offset,
                ),
                _ => None,
            };
            if let Some(name) = nominal {
                let target = if enum_def_by_name(ws, name).is_some() {
                    RefTarget::Enum(name)
                } else if newtype_declared(ws, name) {
                    RefTarget::Newtype(name)
                } else {
                    RefTarget::Struct(name)
                };
                offer(31, target);
            }
            for (expr, node) in body.expr_arena.iter() {
                let selected = selected == Some(NodeRef::Expr(expr));
                match node {
                    hir::Expr::EnumVariant { .. } => {
                        if let Some(token) = self
                            .enum_variant_token(expr)
                            .filter(|token| span_contains(&token.span, offset))
                        {
                            let rank = match token.target {
                                RefTarget::EnumVariant(..) => 1,
                                RefTarget::Enum(_) => 11,
                                _ => 51,
                            };
                            offer(rank, token.target);
                        }
                    }
                    hir::Expr::StructLiteral { name, .. } if selected => {
                        if body.expr_spans.get(expr).is_some_and(|span| {
                            in_leading_name(offset, span, source_ident(*name).len())
                        }) {
                            let target = if enum_def_by_name(ws, *name).is_some() {
                                RefTarget::Enum(*name)
                            } else if newtype_declared(ws, *name) {
                                RefTarget::Newtype(*name)
                            } else {
                                RefTarget::Struct(*name)
                            };
                            offer(31, target);
                        }
                        if let Some(field) = self.literal_field(expr) {
                            offer(41, field.target);
                        }
                    }
                    hir::Expr::FieldAccess { base, field } if selected => {
                        if body
                            .expr_spans
                            .get(expr)
                            .is_some_and(|span| offset + field.len() >= span.offset_range().1)
                        {
                            if let Some(name) = self.bi.and_then(|bi| {
                                types::nominal_name(ws, Some(bi), *bi.expr_types.get(*base)?)
                            }) {
                                offer(41, RefTarget::Field(RecordType::Struct(name), *field));
                            }
                        }
                    }
                    hir::Expr::Global(name) if selected => {
                        if !name.as_str().starts_with("__") {
                            if let Some(&(mid, fid)) = ws
                                .modules
                                .iter()
                                .find_map(|m| m.functions_by_name.get(name).and_then(|f| f.first()))
                            {
                                offer(60, RefTarget::Function(mid, fid));
                            }
                        }
                        if ws
                            .modules
                            .iter()
                            .any(|m| m.global_by_name.contains_key(name))
                        {
                            offer(71, RefTarget::Global(*name));
                        }
                    }
                    _ => {}
                }
                if selected {
                    if let Some(&(mid, fid)) =
                        self.bi.and_then(|bi| bi.resolved_overload_calls.get(&expr))
                    {
                        offer(59, RefTarget::Function(mid, fid));
                    }
                }
            }
            for (id, pattern) in body.pattern_arena.iter() {
                match pattern {
                    hir::Pattern::EnumVariant {
                        enum_name,
                        enum_name_span,
                        variant,
                        variant_span,
                        ..
                    } => {
                        if span_contains(variant_span, offset) {
                            if let Some(en) = enum_def_for_variant(ws, *enum_name, *variant) {
                                offer(2, RefTarget::EnumVariant(en.name, *variant));
                            }
                        }
                        if let (Some(name), Some(span)) = (enum_name, enum_name_span) {
                            if span_contains(span, offset) {
                                offer(12, RefTarget::Enum(*name));
                            }
                        }
                    }
                    hir::Pattern::Record {
                        enum_name: Some(en),
                        enum_name_span,
                        name,
                        name_span,
                        ..
                    } => {
                        if enum_name_span.is_some_and(|span| span_contains(&span, offset)) {
                            offer(12, RefTarget::Enum(*en));
                        }
                        if let Some(variant) = name {
                            if name_span.is_some_and(|span| span_contains(&span, offset)) {
                                offer(2, RefTarget::EnumVariant(*en, *variant));
                            }
                        }
                    }
                    _ => {}
                }
                if let Some(field) = self.pattern_field(id) {
                    let rank = match field.target {
                        RefTarget::Field(RecordType::Struct(_), _) => 42,
                        _ => 52,
                    };
                    offer(rank, field.target);
                }
            }
        }
        found.map(|(_, target)| target)
    }
}

fn decorator_name_span<'t>(def: &hir::DecoratorDef, text_of: TextOf<'_, 't>) -> Option<Span> {
    decl_name_span_in(
        text_of(def.span.file.as_str())?,
        def.span.offset_range().0,
        def.span.offset_range().1,
        def.name.as_str(),
        &def.span,
    )
}

/// Every site (declaration included) of the symbol at `offset`. Shorthand
/// sites carry a [`Shorthand`] tag so rename expands them.
pub fn references<'t>(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
    text_of: TextOf<'_, 't>,
    fn_scope: FnRefScope,
) -> Vec<RefSite> {
    let mut sites = ResolvedPosition::new(ws, mid, Some(inf), offset, NodePolicy::Narrowest)
        .and_then(|at| at.symbol(text_of))
        .map_or_else(Vec::new, |symbol| symbol.sites(ws, mid, text_of, fn_scope));
    // Walks may meet one token twice (a declaration that is also a use);
    // the first sighting wins.
    let mut seen = std::collections::HashSet::new();
    sites.retain(|s| seen.insert((s.span.file, s.span.offset_range())));
    sites
}

/// Rename uses it to decide which names opaque code (an `inline(lua)` body,
/// a `@nomangle` caller) can reach by spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RenameSubject {
    /// A local, parameter or pattern binding.
    Binding,
    /// A function; the name every overload shares.
    Function(Ustr),
    /// A global; emitted under its own name.
    Global(Ustr),
    /// A struct field; emitted as the table key of the same name.
    Field,
    /// A struct (`Some`) or enum name; emitted in runtime type tags.
    Type(Option<Ustr>),
    /// Anything else references resolves (generic parameters, enum variants,
    /// newtypes, record-variant fields, decorators), or nothing.
    Other,
}

pub(crate) fn rename_subject<'t>(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
    text_of: TextOf<'_, 't>,
) -> RenameSubject {
    match ResolvedPosition::new(ws, mid, Some(inf), offset, NodePolicy::Narrowest)
        .and_then(|at| at.symbol(text_of))
    {
        Some(RefTarget::Binding(..)) => RenameSubject::Binding,
        Some(RefTarget::Function(fmid, fid)) => {
            RenameSubject::Function(ws.module(fmid).functions[fid].name)
        }
        Some(RefTarget::Global(name)) => RenameSubject::Global(name),
        Some(RefTarget::Field(RecordType::Struct(_), _)) => RenameSubject::Field,
        Some(RefTarget::Enum(_)) => RenameSubject::Type(None),
        Some(RefTarget::Struct(name)) => RenameSubject::Type(Some(name)),
        _ => RenameSubject::Other,
    }
}

impl RefTarget {
    fn sites<'t>(
        self,
        ws: &Workspace,
        mid: ModuleId,
        text_of: TextOf<'_, 't>,
        scope: FnRefScope,
    ) -> Vec<RefSite> {
        let mut sites = Vec::new();
        let mut plain = |span| sites.push(RefSite::plain(span));
        match self {
            Self::Binding(owner, binding) => {
                let body = &ws.module(mid).bodies[&owner];
                let mut sites = local_references(body, binding);
                if let hir::BodyOwner::Function(fid) = owner
                    && body.binding_arena[binding].kind == hir::BindingKind::Param
                    && body.param_bindings.into_iter().any(|p| p == binding)
                {
                    append_param_named_arg_sites(
                        ws,
                        (mid, fid),
                        body.binding_arena[binding].name,
                        text_of,
                        &mut sites,
                    );
                }
                return sites;
            }
            Self::Function(mid, fid) => {
                let (target, owned_only) = match scope {
                    FnRefScope::ThisOverload => (Some((mid, fid)), false),
                    FnRefScope::AllOverloads => {
                        (None, !is_read_only_source(ws.module(mid).file.as_str()))
                    }
                };
                return function_references_by_name(
                    ws,
                    ws.module(mid).functions[fid].name,
                    target,
                    owned_only,
                    text_of,
                )
                .into_iter()
                .map(RefSite::plain)
                .collect();
            }
            Self::GenericParam(mid, idx) => {
                let Some(module) = ws.modules.get(mid.index()) else {
                    return sites;
                };
                let hir::TypeExpr::Generic(_, name) = module.ty_arena[idx] else {
                    return sites;
                };
                if let Some(span) = generic_decl_span(ws, mid, idx) {
                    plain(leading_name_span(&span, source_ident(name).len()));
                }
            }
            _ => {}
        }
        for module in &ws.modules {
            let decl = match self {
                Self::Struct(name) => module.struct_by_name.get(&name).map(|idx| {
                    leading_name_span(&module.structs[*idx].name_span, source_ident(name).len())
                }),
                Self::Enum(name) => module.enums_by_name.get(&name).map(|idx| {
                    leading_name_span(&module.enums[*idx].name_span, source_ident(name).len())
                }),
                Self::Newtype(name) => module
                    .newtypes_by_name
                    .get(&name)
                    .map(|idx| module.newtypes[*idx].name_span),
                Self::EnumVariant(en, variant) => module
                    .enums_by_name
                    .get(&en)
                    .and_then(|idx| {
                        module.enums[*idx]
                            .variants
                            .iter()
                            .find(|v| v.name == variant)
                    })
                    .map(|v| v.name_span),
                Self::Global(name) => module
                    .global_by_name
                    .get(&name)
                    .map(|idx| module.globals[*idx].name_span),
                Self::Field(record, field) => record
                    .fields(module)
                    .and_then(|fields| fields.iter().find(|f| f.name == field))
                    .map(|f| f.span),
                _ => None,
            };
            if let Some(span) = decl {
                sites.push(RefSite::plain(span));
            }
            if let Self::Decorator(name) = self {
                for def in module.decorators.values().filter(|d| d.def_key == name) {
                    if let Some(span) = decorator_name_span(def, text_of) {
                        sites.push(RefSite::plain(span));
                    }
                }
                for function in module.functions.values() {
                    for annotation in &function.annotations {
                        if annotation.resolved_name.unwrap_or(annotation.name.name) == name {
                            sites.push(RefSite::plain(annotation.name.span));
                        }
                    }
                }
                continue;
            }
            for (owner, body) in &module.bodies {
                let bi = ws
                    .inference(module.id)
                    .and_then(|inf| inf.per_fn.get(owner));
                match self {
                    Self::Struct(name) | Self::Enum(name) | Self::Newtype(name) => {
                        for_each_nominal_annotation(ws, module.file, body, name, &mut |span| {
                            let span = leading_name_span(span, source_ident(name).len());
                            if !matches!(self, Self::Newtype(_))
                                || text_of(module.file.as_str()).is_none_or(|text| {
                                    span_text(text, &span) == Some(source_ident(name))
                                })
                            {
                                sites.push(RefSite::plain(span));
                            }
                        });
                    }
                    Self::GenericParam(mid, idx) => {
                        if mid != module.id {
                            continue;
                        }
                        let hir::TypeExpr::Generic(var, name) = module.ty_arena[idx] else {
                            continue;
                        };
                        let is_param = |idx| matches!(module.ty_arena[idx], hir::TypeExpr::Generic(v, _) if v == var);
                        for iv in body.node_index().iter() {
                            if let NodeRef::StaticTy { mid: ty_mid, idx } = iv.val {
                                if ty_mid == mid
                                    && (is_param(idx)
                                        || matches!(module.ty_arena[idx], hir::TypeExpr::Nullable(inner) if is_param(inner)))
                                {
                                    sites.push(RefSite::plain(leading_name_span(
                                        &Span::new(
                                            module.file,
                                            (0, 0),
                                            (0, 0),
                                            (iv.start, iv.stop),
                                        ),
                                        source_ident(name).len(),
                                    )));
                                }
                            }
                        }
                        continue;
                    }
                    _ => {}
                }
                for (expr, node) in body.expr_arena.iter() {
                    let span = body.expr_spans.get(expr);
                    match (self, node) {
                        (
                            Self::Newtype(name),
                            hir::Expr::Call {
                                callee:
                                    hir::Callee::Unresolved {
                                        name: called,
                                        name_span,
                                    },
                                ..
                            },
                        ) if name == *called => {
                            sites.push(RefSite::plain(body.name_spans[*name_span]))
                        }
                        (Self::Struct(name), hir::Expr::StructLiteral { name: head, .. })
                            if name == *head =>
                        {
                            if let Some(span) = span {
                                sites.push(RefSite::plain(leading_name_span(
                                    span,
                                    source_ident(name).len(),
                                )));
                            }
                        }
                        (
                            Self::Enum(name),
                            hir::Expr::EnumVariant {
                                enum_name,
                                enum_name_span,
                                ..
                            },
                        ) if name == *enum_name => sites.push(RefSite::plain(leading_name_span(
                            &body.name_spans[*enum_name_span],
                            source_ident(name).len(),
                        ))),
                        (
                            Self::EnumVariant(en, variant),
                            hir::Expr::EnumVariant {
                                enum_name,
                                variant: name,
                                variant_span,
                                ..
                            },
                        ) if en == *enum_name && variant == *name => {
                            sites.push(RefSite::plain(body.name_spans[*variant_span]))
                        }
                        (Self::Global(name), hir::Expr::Global(key)) if name == *key => {
                            if let Some(span) = span {
                                let end = span.offset_range().1;
                                sites.push(RefSite::plain(
                                    span.with_offset_range((end - source_ident(name).len(), end)),
                                ));
                            }
                        }
                        (Self::Field(record, field), _) => {
                            if let Some((_, fields)) =
                                RecordType::literal(node).filter(|(owner, _)| *owner == record)
                            {
                                for idx in fields {
                                    let f = &body.struct_lit_field_arena[idx];
                                    if f.name == field {
                                        let shorthand =
                                            body.expr_spans.get(f.value).is_some_and(|span| {
                                                span.offset_range() == f.name_span.offset_range()
                                            });
                                        sites.push(RefSite {
                                            span: f.name_span,
                                            shorthand: shorthand
                                                .then_some(Shorthand::KeepBinding(field)),
                                        });
                                    }
                                }
                            } else if let RecordType::Struct(name) = record {
                                if field_access_matches(
                                    ws, module, *owner, bi, expr, node, name, field,
                                ) {
                                    if let Some(span) = span {
                                        let end = span.offset_range().1;
                                        sites.push(RefSite::plain(
                                            span.with_offset_range((end - field.len(), end)),
                                        ));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                for (id, pattern) in body.pattern_arena.iter() {
                    match (self, pattern) {
                        (
                            Self::Enum(name),
                            hir::Pattern::EnumVariant {
                                enum_name: Some(en),
                                enum_name_span: Some(span),
                                ..
                            }
                            | hir::Pattern::Record {
                                enum_name: Some(en),
                                enum_name_span: Some(span),
                                ..
                            },
                        ) if name == *en => sites.push(RefSite::plain(leading_name_span(
                            span,
                            source_ident(name).len(),
                        ))),
                        (
                            Self::EnumVariant(en, variant),
                            hir::Pattern::EnumVariant {
                                enum_name,
                                variant: name,
                                variant_span,
                                ..
                            },
                        ) if variant == *name
                            && enum_def_for_variant(ws, *enum_name, *name)
                                .is_some_and(|def| def.name == en) =>
                        {
                            sites.push(RefSite::plain(*variant_span))
                        }
                        (
                            Self::EnumVariant(en, variant),
                            hir::Pattern::Record {
                                enum_name: Some(name),
                                name: Some(v),
                                name_span: Some(span),
                                ..
                            },
                        ) if en == *name && variant == *v => sites.push(RefSite::plain(*span)),
                        (Self::Field(record, field), _) => {
                            let Some((_, fields)) = RecordType::pattern(ws, body, bi, id)
                                .filter(|(owner, _)| *owner == record)
                            else {
                                continue;
                            };
                            for idx in fields {
                                let f = &body.record_pat_field_arena[idx];
                                if f.name == field {
                                    sites.push(RefSite {
                                        span: f.name_span,
                                        shorthand: pattern_field_is_shorthand(body, f)
                                            .then_some(Shorthand::KeepBinding(field)),
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        sites
    }
}

fn newtype_declared(ws: &Workspace, name: Ustr) -> bool {
    ws.modules
        .iter()
        .any(|m| m.newtypes_by_name.contains_key(&name))
}

fn decorator_declared(ws: &Workspace, name: Ustr) -> bool {
    ws.modules
        .iter()
        .any(|m| m.decorators.values().any(|d| d.def_key == name))
}

pub(crate) fn is_test_path(path: &str) -> bool {
    path.split(['/', '\\']).any(|seg| seg == "tests")
}

fn in_leading_name(offset: usize, span: &Span, len: usize) -> bool {
    let start = span.offset_range().0;
    offset >= start && offset < start + len
}

fn leading_name_span(span: &Span, len: usize) -> Span {
    let start = span.offset_range().0;
    span.with_offset_range((start, start + len))
}

fn ctor_name_if_on(te: &hir::TypeExpr, span: Option<&Span>, offset: usize) -> Option<Ustr> {
    if let hir::TypeExpr::App {
        ctor: hir::TypeCtor::Nominal(name),
        ..
    } = te
    {
        if in_leading_name(offset, span?, source_ident(*name).len()) {
            return Some(*name);
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecordType {
    Struct(Ustr),
    Variant(Ustr, Ustr),
}

#[derive(Clone, Copy)]
pub(crate) struct Token {
    target: RefTarget,
    span: Span,
}

impl Token {
    pub(crate) fn declaration(self, ws: &Workspace) -> Option<Span> {
        self.target.declaration(ws)
    }

    pub(crate) fn hover(self, ws: &Workspace) -> Option<(String, Span)> {
        Some((self.target.hover(ws)?, self.span))
    }
}

impl RefTarget {
    fn declaration(self, ws: &Workspace) -> Option<Span> {
        match self {
            Self::EnumVariant(en, variant) => variant_def_span(ws, Some(en), variant),
            Self::Enum(name) => def_of_name(ws, name),
            Self::Struct(name) => struct_def_by_name(ws, name).map(|def| def.name_span),
            Self::Field(record, name) => Some(record.field(ws, name)?.span),
            _ => None,
        }
    }

    fn hover(self, ws: &Workspace) -> Option<String> {
        match self {
            Self::EnumVariant(en, variant) => render_variant(ws, Some(en), variant),
            Self::Enum(name) => render_enum_def(ws, name),
            Self::Struct(name) => render_struct_def(ws, name),
            Self::Field(record, name) => record.render_field(ws, name),
            _ => None,
        }
    }
}

impl RecordType {
    fn literal(expr: &hir::Expr) -> Option<(Self, IdxRange<hir::StructLitField>)> {
        match expr {
            hir::Expr::StructLiteral { name, fields, .. } => Some((Self::Struct(*name), *fields)),
            hir::Expr::EnumVariant {
                enum_name,
                variant,
                args: hir::VariantArgs::Record(fields),
                ..
            } => Some((Self::Variant(*enum_name, *variant), *fields)),
            _ => None,
        }
    }

    fn pattern(
        ws: &Workspace,
        body: &Body,
        bi: Option<&BodyInference>,
        id: Idx<hir::Pattern>,
    ) -> Option<(Self, IdxRange<hir::RecordPatField>)> {
        let hir::Pattern::Record {
            enum_name,
            name,
            fields,
            ..
        } = body.pattern_arena[id]
        else {
            return None;
        };
        if let Some(en) = enum_name {
            return Some((Self::Variant(en, name?), fields));
        }
        let bi = bi?;
        let (ty, node) = ws.resolve_ty(Some(bi), *bi.record_pattern_types.get(&id)?)?;
        let ty = match node {
            hir::TypeExpr::App {
                ctor: hir::TypeCtor::Ref,
                params,
            } => ty.with_idx(params.into_iter().next()?),
            _ => ty,
        };
        let owner = types::member_owner_name(ws, Some(bi), ty)?;
        let record = if ws.enums_by_name.contains_key(&owner) {
            Self::Variant(owner, ustr(source_ident(name?)))
        } else {
            Self::Struct(owner)
        };
        Some((record, fields))
    }

    fn field(self, ws: &Workspace, name: Ustr) -> Option<&hir::StructFieldDef> {
        ws.modules
            .iter()
            .find_map(|module| self.fields(module))?
            .iter()
            .find(|field| field.name == name)
    }

    fn render_field(self, ws: &Workspace, name: Ustr) -> Option<String> {
        let ty = ws.render(None, self.field(ws, name)?.ty);
        let owner = match self {
            Self::Struct(name) => display_name(name.as_str()).to_owned(),
            Self::Variant(en, variant) => format!("{}::{variant}", display_name(en.as_str())),
        };
        Some(format!("{name}: {ty} (field of {owner})"))
    }

    fn fields(self, module: &hir::Module) -> Option<&[hir::StructFieldDef]> {
        match self {
            Self::Struct(name) => Some(&module.structs[*module.struct_by_name.get(&name)?].fields),
            Self::Variant(name, variant) => {
                let def = &module.enums[*module.enums_by_name.get(&name)?];
                let v = def.variants.iter().find(|v| v.name == variant)?;
                match &v.payload {
                    hir::VariantPayload::Record(fields) => Some(fields),
                    _ => None,
                }
            }
        }
    }
}

fn pattern_field_is_shorthand(body: &Body, field: &hir::RecordPatField) -> bool {
    match body.pattern_arena[field.pat] {
        hir::Pattern::Bind(b) => body
            .binding_spans
            .get(b)
            .is_some_and(|span| span.offset_range() == field.name_span.offset_range()),
        _ => false,
    }
}

fn field_access_matches(
    ws: &Workspace,
    module: &hir::Module,
    owner: hir::BodyOwner,
    bi: Option<&BodyInference>,
    e: Idx<hir::Expr>,
    expr: &hir::Expr,
    name: Ustr,
    field: Ustr,
) -> bool {
    match expr {
        hir::Expr::FieldAccess { base, field: fname }
        | hir::Expr::SafeNav { base, field: fname }
            if *fname == field =>
        {
            bi.and_then(|bi| types::nominal_name(ws, Some(bi), *bi.expr_types.get(*base)?))
                == Some(name)
        }
        hir::Expr::SiblingRef(n) if *n == field => match owner {
            hir::BodyOwner::StructDefaults(sidx) => module.structs[sidx].name == name,
            _ => bi.and_then(|bi| field_ref_struct(ws, bi, *bi.expr_types.get(e)?)) == Some(name),
        },
        _ => false,
    }
}

/// Walks `node_index`, not `ty_arena`: `ty_spans` skips bare `Ident`-kind
/// types, so arena iteration misses a `p: Point` annotation.
fn for_each_nominal_annotation(
    ws: &Workspace,
    file: Ustr,
    body: &hir::Body,
    name: Ustr,
    f: &mut impl FnMut(&Span),
) {
    for iv in body.node_index().iter() {
        let nominal = match iv.val {
            NodeRef::StaticTy { mid, idx } => ws
                .modules
                .get(mid.index())
                .and_then(|module| types::annotation_nominal(&module.ty_arena, idx)),
            NodeRef::LocalTy(idx) => types::annotation_nominal(&body.lowered_tys, idx),
            _ => None,
        };
        let hit = nominal == Some(name);
        if hit {
            f(&Span::new(file, (0, 0), (0, 0), (iv.start, iv.stop)));
        }
    }
}

pub(crate) fn span_contains(span: &Span, offset: usize) -> bool {
    let (s, e) = span.offset_range();
    s <= offset && offset < e
}

pub(crate) fn span_text<'t>(text: &'t str, span: &Span) -> Option<&'t str> {
    let (s, e) = span.offset_range();
    text.get(s..e)
}

pub(crate) fn declared_generics(module: &hir::Module) -> impl Iterator<Item = &TyId> {
    module
        .functions
        .values()
        .flat_map(|f| &f.generics)
        .chain(module.structs.values().flat_map(|s| &s.generics))
        .chain(module.enums.values().flat_map(|e| &e.generics))
}

/// The `<T>` name that declares the generic at `(gmid, gidx)`, which may be a
/// copy of the declared slot.
fn generic_decl_span(ws: &Workspace, gmid: ModuleId, gidx: Idx<hir::TypeExpr>) -> Option<Span> {
    let m = ws.modules.get(gmid.index())?;
    let hir::TypeExpr::Generic(var, _) = m.ty_arena[gidx] else {
        return None;
    };
    declared_generics(m).find_map(|g| match g {
        TyId::Static(gm, gi)
            if *gm == gmid
                && matches!(m.ty_arena[*gi], hir::TypeExpr::Generic(v, _) if v == var) =>
        {
            m.ty_spans.get(*gi).copied()
        }
        _ => None,
    })
}

pub(crate) fn generic_param_def(ws: &Workspace, mid: ModuleId, offset: usize) -> Option<Span> {
    let (gmid, gidx) =
        ResolvedPosition::new(ws, mid, None, offset, NodePolicy::Narrowest)?.generic()?;
    generic_decl_span(ws, gmid, gidx)
}

fn local_references(body: &Body, b: Idx<hir::BindingInfo>) -> Vec<RefSite> {
    let name = body.binding_arena[b].name;
    let name_len = source_ident(name).len();

    // Shorthand iff a same-named field's name token is this site's token;
    // explicit `x: x` has the field name on a different token.
    let shorthand_at = |sp: &Span| -> Option<Shorthand> {
        let lit = body
            .struct_lit_field_arena
            .values()
            .any(|f| f.name == name && f.name_span.offset_range() == sp.offset_range());
        let pat = body
            .record_pat_field_arena
            .values()
            .any(|f| f.name == name && f.name_span.offset_range() == sp.offset_range());
        (lit || pat).then_some(Shorthand::KeepField(name))
    };

    let mut out = Vec::new();

    if let Some(s) = body.binding_spans.get(b) {
        // A param's binding span covers `name: type`; clip to the name.
        let start = s.offset_range().0;
        let span = s.with_offset_range((start, start + name_len));
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
                    let start = s.offset_range().0;
                    out.push(RefSite::plain(
                        s.with_offset_range((start, start + name_len)),
                    ));
                }
            }

            _ => {}
        }
    }

    out
}

fn named_arguments(body: &Body, call: Idx<hir::Expr>) -> impl Iterator<Item = (Ustr, &Span)> {
    let args = match body.expr_arena[call] {
        hir::Expr::Call { args, .. } | hir::Expr::SafeNavCall { args, .. } => Some(args),
        _ => None,
    };
    args.into_iter()
        .flatten()
        .filter_map(|arg| Some((*body.arg_names.get(&arg)?, body.expr_spans.get(arg)?)))
}

/// HIR keeps label names but no spans. The label is immediately before its value.
fn named_argument_span(text_of: TextOf<'_, '_>, label: Ustr, value: &Span) -> Option<Span> {
    named_label_before(
        text_of(value.file.as_str())?,
        value.offset_range().0,
        label.as_str(),
        value,
    )
}

fn append_param_named_arg_sites(
    ws: &Workspace,
    target: (ModuleId, Idx<Function>),
    name: Ustr,
    text_of: TextOf<'_, '_>,
    out: &mut Vec<RefSite>,
) {
    for (mid, inf) in ws.inferences_iter() {
        let Some(module) = ws.modules.get(mid.index()) else {
            continue;
        };
        for (owner, bi) in inf.per_fn.iter() {
            let Some(body) = module.bodies.get(owner) else {
                continue;
            };
            for (&call, &resolved) in &bi.resolved_overload_calls {
                if resolved == target {
                    out.extend(
                        named_arguments(body, call)
                            .filter(|&(label, _)| label == name)
                            .filter_map(|(label, value)| named_argument_span(text_of, label, value))
                            .map(RefSite::plain),
                    );
                }
            }
        }
    }
}

pub(crate) fn named_arg_param_at(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
    text_of: TextOf<'_, '_>,
) -> Option<Span> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let bi = inf.per_fn.get(&owner)?;
    for (&call, &(mid, fid)) in &bi.resolved_overload_calls {
        let Some((label, _)) = named_arguments(body, call).find(|&(label, value)| {
            named_argument_span(text_of, label, value)
                .is_some_and(|span| span_contains(&span, offset))
        }) else {
            continue;
        };
        let target = ws.modules.get(mid.index())?;
        let body = target.bodies.get(&hir::BodyOwner::Function(fid))?;
        let param = body
            .param_bindings
            .into_iter()
            .find(|p| body.binding_arena[*p].name == label)?;
        return Some(leading_name_span(
            body.binding_spans.get(param)?,
            label.len(),
        ));
    }
    None
}

fn node_at(body: &hir::Body, offset: usize) -> Option<NodeRef> {
    body.node_index()
        .find(offset, offset + 1)
        .min_by_key(|x| x.stop - x.start)
        .map(|iv| iv.val)
}

fn binding_at(body: &hir::Body, offset: usize) -> Option<Idx<hir::BindingInfo>> {
    binding_of_node(body, node_at(body, offset)?)
}

fn binding_of_node(body: &Body, node: NodeRef) -> Option<Idx<hir::BindingInfo>> {
    match node {
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
    }
}

/// `None` unless the text before the value is exactly `label`, ws, `:`,
/// ws (fail open: no edit beats a wrong edit).
fn named_label_before(text: &str, value_start: usize, label: &str, like: &Span) -> Option<Span> {
    let b = text.as_bytes();
    let mut i = value_start.min(b.len());
    while i > 0 && b[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 || b[i - 1] != b':' {
        return None;
    }
    i -= 1;
    while i > 0 && b[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    let end = i;
    let start = end.checked_sub(label.len())?;
    if text.get(start..end) != Some(label) {
        return None;
    }
    if start > 0 && (b[start - 1] == b'_' || b[start - 1].is_ascii_alphanumeric()) {
        return None;
    }
    Some(like.with_offset_range((start, end)))
}

fn ident_span_in(text: &str, start: usize, end: usize, ident: &str, like: &Span) -> Option<Span> {
    ident_span_where(text, start, end, ident, like, false)
}

fn decl_name_span_in(
    text: &str,
    start: usize,
    end: usize,
    ident: &str,
    like: &Span,
) -> Option<Span> {
    ident_span_where(text, start, end, ident, like, true)
}

fn ident_span_where(
    text: &str,
    start: usize,
    end: usize,
    ident: &str,
    like: &Span,
    before_open: bool,
) -> Option<Span> {
    let hay = text.get(start..end.min(text.len()))?;
    let hb = hay.as_bytes();
    let is_ident = |c: u8| c == b'_' || c.is_ascii_alphanumeric();
    let mut from = 0;
    while let Some(p) = hay[from..].find(ident) {
        let abs = from + p;
        let after = abs + ident.len();
        let before_ok = abs == 0 || !is_ident(hb[abs - 1]);
        let after_ok = after >= hb.len() || !is_ident(hb[after]);
        let open_ok = !before_open || {
            let mut i = after;
            while i < hb.len() && hb[i].is_ascii_whitespace() {
                i += 1;
            }
            i < hb.len() && (hb[i] == b'(' || hb[i] == b'{')
        };
        if before_ok && after_ok && open_ok {
            return Some(like.with_offset_range((start + abs, start + after)));
        }
        from = abs + 1;
    }
    None
}

fn function_references_by_name<'t>(
    ws: &Workspace,
    name: Ustr,
    target: Option<(ModuleId, Idx<hir::Function>)>,
    owned_only: bool,
    text_of: TextOf<'_, 't>,
) -> Vec<Span> {
    let mut out = Vec::new();
    let excluded = |m: &hir::Module| owned_only && is_read_only_source(m.file.as_str());

    for m in ws.modules.iter() {
        if let Some(fns) = m.functions_by_name.get(&name) {
            for (fmid, fid) in fns.iter() {
                if target.is_some_and(|t| t != (*fmid, *fid)) {
                    continue;
                }
                if let Some(fm) = ws.modules.get(fmid.index()).filter(|fm| !excluded(fm)) {
                    out.push(fm.functions[*fid].name_span);
                }
            }
        }
    }

    for (m, inf) in ws.inferences_iter() {
        let Some(module) = ws.modules.get(m.index()) else {
            continue;
        };

        for (owner, bi) in inf.per_fn.iter() {
            let Some(body) = module.bodies.get(owner) else {
                continue;
            };

            for (call_idx, (cmid, cfid)) in bi.resolved_overload_calls.iter() {
                let matches = match target {
                    Some(t) => (*cmid, *cfid) == t,
                    None => ws
                        .modules
                        .get(cmid.index())
                        .is_some_and(|cm| cm.functions[*cfid].name == name && !excluded(cm)),
                };
                if matches {
                    match &body.expr_arena[*call_idx] {
                        hir::Expr::Call {
                            callee: hir::Callee::Unresolved { name_span, .. },
                            ..
                        } => out.push(body.name_spans[*name_span]),
                        // HIR keeps no method-token span for `recv?.method()`; find `name` in
                        // source after the receiver.
                        hir::Expr::SafeNavCall { receiver, .. } => {
                            let (Some(sp), Some(rsp)) = (
                                body.expr_spans.get(*call_idx),
                                body.expr_spans.get(*receiver),
                            ) else {
                                continue;
                            };
                            let Some(text) = text_of(sp.file.as_str()) else {
                                continue;
                            };
                            if let Some(span) = ident_span_in(
                                text,
                                rsp.offset_range().1,
                                sp.offset_range().1,
                                source_ident(name),
                                sp,
                            ) {
                                out.push(span);
                            }
                        }
                        _ => {}
                    }
                }
            }

            // A bare top-level name used as a value has no argument list to pick an
            // overload, so both scopes match it by name.
            if excluded(module) {
                continue;
            }
            for (e, expr) in body.expr_arena.iter() {
                if let hir::Expr::Global(gname) = expr {
                    if *gname == name {
                        if let Some(sp) = body.expr_spans.get(e) {
                            out.push(*sp);
                        }
                    }
                }
            }
        }
    }

    out
}

pub(crate) fn inferred_let_bindings(
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
            let s = span.offset_range().0;
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

pub(crate) fn call_param_hints(
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

            for (i, arg) in (*args).enumerate() {
                // Named args reorder params; positional args are a contiguous prefix,
                // so index == param index for them.
                if body.arg_names.contains_key(&arg) {
                    continue;
                }
                let Some(param) = f.param_decls.get(i) else {
                    break;
                };
                let Some(sp) = body.expr_spans.get(arg) else {
                    continue;
                };
                let s = sp.offset_range().0;
                if s >= start && s < end {
                    out.push((*sp, format!("{}:", param.name)));
                }
            }
        }
    }

    out
}

/// Read D-form receiver types from the AST because lowering drops the surface type.
pub(crate) fn receiver_param_hints(ast: &Ast, start: usize, end: usize) -> Vec<(Span, String)> {
    let mut out = Vec::new();
    for item in &ast.items {
        let skull_parser::ItemKind::Function(f) = &item.kind else {
            continue;
        };
        if f.receiver_surface.is_none() {
            continue;
        }
        let Some(recv) = f.params.first() else {
            continue;
        };
        let s = recv.span.offset_range().0;
        if s < start || s >= end {
            continue;
        }
        out.push((recv.span, format!(": {}", render_type_expr(ast, recv.ty))));
    }
    out
}

fn render_type_expr(ast: &Ast, idx: skull_parser::Idx<skull_parser::TypeExprData>) -> String {
    use skull_parser::TypeExprKind as K;
    let te = ast.te(&idx);
    let join = |xs: &[skull_parser::Idx<skull_parser::TypeExprData>]| {
        xs.iter()
            .map(|x| render_type_expr(ast, *x))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let base = match &te.kind {
        K::Any => "any".to_string(),
        K::Unit => "()".to_string(),
        K::String => "string".to_string(),
        K::Int => "int".to_string(),
        K::Uint => "uint".to_string(),
        K::Float => "float".to_string(),
        K::F64 => "f64".to_string(),
        K::F32 => "f32".to_string(),
        K::Bool => "bool".to_string(),
        K::Ref(target) => format!("ref {}", render_type_expr(ast, *target)),
        K::Ident(id) => id.name.to_string(),
        K::Path(path) => {
            let root = if path.from_root { "::" } else { "" };
            format!(
                "{root}{}",
                path.segments
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join("::")
            )
        }
        K::Array(el) => format!("[{}]", render_type_expr(ast, *el)),
        K::AppPath { ctor, args } => {
            let root = if ctor.from_root { "::" } else { "" };
            format!(
                "{root}{}<{}>",
                ctor.segments
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join("::"),
                join(args)
            )
        }
        K::App { ctor, args } => format!("{}<{}>", ctor.name, join(args)),
        K::Tuple(els) => format!("({})", join(els)),
        K::Function { params, ret } => {
            format!("fn({}) -> {}", join(params), render_type_expr(ast, *ret))
        }
        K::Hole(..) => "?".to_string(),
    };
    if te.is_nullable {
        format!("{base}?")
    } else {
        base
    }
}

pub(crate) fn locals_in_scope(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Vec<(Ustr, hir::BindingKind, String, Option<TyId>, usize)> {
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

    // A binding is invisible inside its initializer, where an outer name may still resolve.
    let mut init_end: std::collections::HashMap<Idx<hir::BindingInfo>, usize> = Default::default();
    for (_, stmt) in body.statement_arena.iter() {
        let (leaves, value) = match stmt {
            hir::Statement::Let { pattern, value, .. } => (body.pattern_bindings(*pattern), *value),
            _ => continue,
        };
        if let Some(end) = body.expr_spans.get(value).map(|s| s.offset_range().1) {
            for b in leaves {
                init_end.insert(b, end);
            }
        }
    }

    for (b_idx, binding) in body.binding_arena.iter() {
        let in_scope = match binding.kind {
            hir::BindingKind::Param => true,
            hir::BindingKind::Let | hir::BindingKind::Static | hir::BindingKind::Fn => {
                match init_end.get(&b_idx) {
                    Some(end) => *end <= offset,
                    None => body
                        .binding_spans
                        .get(b_idx)
                        .is_some_and(|s| s.offset_range().1 <= offset),
                }
            }
        };
        if !in_scope {
            continue;
        }

        // A `TyId::Local` here indexes the cursor body's arena, which an
        // `ExpectedArg` scratch Gamma clones, so it stays resolvable there.
        let ty_id = bi.and_then(|bi| bi.binding_types.get(b_idx).copied());
        let ty = match (bi, ty_id) {
            (Some(bi), Some(t)) => ws.render(Some(bi), t),
            _ => String::new(),
        };
        let decl_end = body
            .binding_spans
            .get(b_idx)
            .map_or(0, |s| s.offset_range().1);
        out.push((binding.name, binding.kind, ty, ty_id, decl_end));
    }

    out
}

/// A substituted field type may lack a TyId.
fn struct_fields_of_ty(
    ws: &Workspace,
    mid: ModuleId,
    offset: usize,
    recv: TyRef<'_>,
) -> Option<(Vec<(String, String, Option<TyId>)>, Ustr)> {
    let typed = ws.inference(mid).and_then(|inf| {
        let (mut gamma, span) = query_gamma_for(ws, mid, inf, offset, recv)?;
        let (name, fields) = inference::struct_fields_of(&mut gamma, recv.ty, span)?;
        let in_caller_arena = |t: TyId| match t {
            TyId::Static(..) => true,
            TyId::Local(idx) => recv
                .body
                .is_some_and(|b| (idx.into_raw().into_u32() as usize) < b.ty_arena.len()),
        };
        let fields = fields
            .into_iter()
            .map(|(n, t)| {
                (
                    n.to_string(),
                    gamma.render(t),
                    Some(t).filter(|&t| in_caller_arena(t)),
                )
            })
            .collect();
        Some((fields, name))
    });
    if typed.is_some() {
        return typed;
    }
    let sname = types::member_owner_name(ws, recv.body, recv.ty)?;
    let sdef = struct_def_by_name(ws, sname)?;
    let fields = sdef
        .fields
        .iter()
        .map(|f| (f.name.to_string(), ws.render(None, f.ty), Some(f.ty)))
        .collect();
    Some((fields, sname))
}

/// A `TyId::Local` only resolves in the scratch Gamma seeded from its own body.
fn query_gamma_for<'a>(
    ws: &'a Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
    recv: TyRef<'_>,
) -> Option<(inference::Gamma<'a>, Span)> {
    if let TyId::Local(_) = recv.ty {
        let seed = ws
            .modules
            .get(mid.index())
            .and_then(|m| m.body_at(offset))
            .and_then(|o| inf.per_fn.get(&o));
        if !matches!((seed, recv.body), (Some(a), Some(b)) if std::ptr::eq(a, b)) {
            return None;
        }
    }
    crate::backend::query_gamma(ws, mid, inf, offset)
}

pub(crate) struct Members<'a> {
    /// Name, rendered type, and the type id when valid in the caller's arena.
    pub fields: Vec<(String, String, Option<TyId>)>,
    /// The nominal its UFCS functions take first (`Vec` for `[T]`).
    pub owner: Ustr,
    pub vec_elem: Option<TyRef<'a>>,
    /// The struct a one-argument wrapper carries (`Entity<Glyph>` -> `Glyph`).
    pub sole_arg: Option<Ustr>,
    pub is_iter: bool,
}

pub(crate) fn members_of_receiver_expr(
    ws: &Workspace,
    mid: ModuleId,
    recv_end: usize,
) -> Option<Members<'_>> {
    let inf = ws.inference(mid)?;
    let at = recv_end.checked_sub(1)?;
    let ta = bone_semantic::query::type_at(ws, mid, inf, at)?;
    members_of_ty(ws, mid, at, TyRef::new(ta.body, ta.ty))
}

/// Serves receivers inference left untyped, such as an argument of an
/// unknown call (`foo(a.b.|`).
pub(crate) fn members_of_name_path<'a>(
    ws: &'a Workspace,
    mid: ModuleId,
    offset: usize,
    segments: &[String],
) -> Option<Members<'a>> {
    let (head, rest) = segments.split_first()?;
    let mut recv = binding_or_global_ty(ws, mid, offset, head)?;
    for seg in rest {
        recv = member_ty(ws, recv, seg)?;
    }
    members_of_ty(ws, mid, offset, recv)
}

fn members_of_ty<'a>(
    ws: &'a Workspace,
    mid: ModuleId,
    offset: usize,
    recv: TyRef<'a>,
) -> Option<Members<'a>> {
    let (fields, owner) = match struct_fields_of_ty(ws, mid, offset, recv) {
        Some(typed) => typed,
        None => (
            Vec::new(),
            types::member_owner_name(ws, recv.body, recv.ty)?,
        ),
    };
    let is_iter = ws
        .inference(mid)
        .is_some_and(|inf| receiver_is_iterator(ws, mid, inf, offset, recv));
    Some(Members {
        fields,
        owner,
        vec_elem: types::array_elem(ws, recv.body, recv.ty).map(|e| TyRef::new(recv.body, e)),
        sole_arg: types::sole_arg_name(ws, recv.body, recv.ty),
        is_iter,
    })
}

fn binding_or_global_ty<'a>(
    ws: &'a Workspace,
    mid: ModuleId,
    offset: usize,
    head: &str,
) -> Option<TyRef<'a>> {
    let module = ws.modules.get(mid.index())?;
    let local = module.body_at(offset).and_then(|owner| {
        let body = module.bodies.get(&owner)?;
        let b = find_binding_by_name(body, head, offset)?;
        let bi = ws.inference(mid).and_then(|i| i.per_fn.get(&owner));
        let inferred = bi.and_then(|bi| bi.binding_types.get(b).copied());
        // A `let` annotation is `Local` to the body's lowered types, which seed
        // its inference arena.
        let ty = inferred.or(body.binding_arena[b].declared_ty)?;
        Some(TyRef::new(bi, ty))
    });
    local.or_else(|| {
        let frag = crate::modules_scope::fragment(ws, module.file)?;
        let key = bone_semantic::lower::resolve::resolve_bare(ws, frag, Ustr::from(head))
            .ok()?
            .key;
        let ty = *ws.globals_by_name.get(&key)?;
        Some(TyRef::of_static(ty))
    })
}

fn member_ty<'a>(ws: &Workspace, recv: TyRef<'a>, seg: &str) -> Option<TyRef<'a>> {
    match types::peel_nullable(ws, recv.body, recv.ty)? {
        (id, hir::TypeExpr::Tuple { elements }) => {
            let elem = elements.into_iter().nth(seg.parse().ok()?)?;
            Some(TyRef::new(recv.body, id.with_idx(elem)))
        }
        (
            _,
            hir::TypeExpr::App {
                ctor: hir::TypeCtor::Nominal(name),
                ..
            },
        ) => {
            if let Some(nt) = newtype_def_by_name(ws, name) {
                return (seg == "0").then(|| TyRef::of_static(nt.wrapped));
            }
            let sdef = struct_def_by_name(ws, name)?;
            let field = sdef.fields.iter().find(|f| f.name.as_str() == seg)?;
            Some(TyRef::of_static(field.ty))
        }
        _ => None,
    }
}

fn field_ref_struct(ws: &Workspace, bi: &BodyInference, ty: TyId) -> Option<Ustr> {
    let (ctor, mut args) = types::nominal_args(ws, Some(bi), ty)?;
    if !is_field_ref_former(ctor) {
        return None;
    }
    types::nominal_name(ws, Some(bi), args.next()?)
}

/// An assign target is typed `()` by inference and is not indexed, so peel
/// the enclosing `Assign` to its target.
pub(crate) fn var_binding_ty_at(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Option<TyId> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let bi = inf.per_fn.get(&owner)?;
    for iv in body.node_index().find(offset, offset + 1) {
        let NodeRef::Expr(e) = iv.val else { continue };
        let target = match &body.expr_arena[e] {
            hir::Expr::Var(_) => e,
            hir::Expr::Assign { target, .. } | hir::Expr::CompoundAssign { target, .. } => {
                if !span_contains(body.expr_spans.get(*target)?, offset) {
                    continue;
                }
                *target
            }
            _ => continue,
        };
        if let hir::Expr::Var(b) = &body.expr_arena[target] {
            return bi.binding_types.get(*b).copied();
        }
    }
    None
}

/// Resolves by name, so it works when the statement does not type-check
/// (`dest = |`).
pub(crate) fn binding_ty_by_name(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
    name: &str,
) -> Option<TyId> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let bi = inf.per_fn.get(&owner)?;
    let b = find_binding_by_name(body, name, offset)?;
    bi.binding_types.get(b).copied()
}

fn find_binding_by_name(body: &Body, head: &str, offset: usize) -> Option<Idx<hir::BindingInfo>> {
    let mut best: Option<(usize, Idx<hir::BindingInfo>)> = None;
    for (idx, b) in body.binding_arena.iter() {
        if b.name.as_str() != head {
            continue;
        }
        let in_scope = match b.kind {
            hir::BindingKind::Param => true,
            hir::BindingKind::Let | hir::BindingKind::Static | hir::BindingKind::Fn => body
                .binding_spans
                .get(idx)
                .is_some_and(|s| s.offset_range().1 <= offset),
        };
        if !in_scope {
            continue;
        }
        let pos = body
            .binding_spans
            .get(idx)
            .map_or(0, |s| s.offset_range().0);
        if best.map_or(true, |(bp, _)| pos >= bp) {
            best = Some((pos, idx));
        }
    }
    best.map(|(_, idx)| idx)
}

/// One-segment record paths stay with HIR, which can distinguish a struct
/// from a variant.
pub(crate) fn enum_variant_pattern_at(
    ws: &Workspace,
    mid: ModuleId,
    ast: &skull_parser::Ast,
    offset: usize,
) -> Option<Token> {
    fn path_variant(
        ws: &Workspace,
        mid: ModuleId,
        path: &skull_parser::Path,
    ) -> Option<(Ustr, Ustr)> {
        let (variant, enum_path) = path.segments.split_last()?;
        if enum_path.is_empty() {
            return None;
        }
        let file = ws.module(mid).file;
        let frag = crate::modules_scope::fragment(ws, file)?;
        let resolved = bone_semantic::lower::resolve::resolve_path(
            ws,
            frag,
            &UsePathRef {
                from_root: path.from_root,
                segments: enum_path.iter().map(|segment| segment.name).collect(),
            },
        )
        .ok()?;
        let enum_def = enum_def_by_name(ws, resolved.key)?;
        enum_def
            .variants
            .iter()
            .any(|candidate| candidate.name == variant.name)
            .then_some((enum_def.name, variant.name))
    }

    fn enum_path_names(
        path: &skull_parser::Path,
    ) -> Option<(&skull_parser::Ident, &skull_parser::Ident)> {
        let (variant, enum_path) = path.segments.split_last()?;
        Some((enum_path.last()?, variant))
    }

    fn pattern_enum_name(ws: &Workspace, mid: ModuleId, name_span: Span) -> Option<Option<Ustr>> {
        let module = ws.modules.get(mid.index())?;
        let owner = module.body_at(name_span.offset_range().0)?;
        let body = module.bodies.get(&owner)?;
        body.pattern_arena
            .values()
            .find_map(|pattern| match pattern {
                hir::Pattern::EnumVariant {
                    enum_name,
                    variant_span,
                    ..
                } if *variant_span == name_span => Some(*enum_name),
                hir::Pattern::Record {
                    enum_name,
                    name_span: Some(span),
                    ..
                } if *span == name_span => Some(*enum_name),
                _ => None,
            })
    }

    fn record_pattern_is_type(ws: &Workspace, mid: ModuleId, name_span: Span) -> Option<bool> {
        let module = ws.modules.get(mid.index())?;
        let owner = module.body_at(name_span.offset_range().0)?;
        let body = module.bodies.get(&owner)?;
        body.pattern_arena
            .values()
            .find_map(|pattern| match pattern {
                hir::Pattern::Record {
                    name,
                    name_span: Some(span),
                    ..
                } if *span == name_span => {
                    Some(name.is_some_and(|name| ws.struct_by_name.contains_key(&name)))
                }
                _ => None,
            })
    }

    fn variant_token(
        ws: &Workspace,
        enum_name: Option<Ustr>,
        variant: &skull_parser::Ident,
    ) -> Option<Token> {
        let en = enum_def_for_variant(ws, enum_name, variant.name)?.name;
        Some(Token {
            target: RefTarget::EnumVariant(en, variant.name),
            span: variant.span,
        })
    }

    fn in_pattern(
        ws: &Workspace,
        mid: ModuleId,
        pat: &skull_parser::Pattern,
        offset: usize,
    ) -> Option<Token> {
        use skull_parser::Pattern;
        let (path, subs, record) = match pat {
            Pattern::PathVariant { path, args } => {
                (path, args.iter().flatten().collect::<Vec<_>>(), false)
            }
            Pattern::PathRecord { path, fields, .. } => (
                path,
                fields.iter().map(|(_, sub)| sub).collect::<Vec<_>>(),
                true,
            ),
            Pattern::Tuple(ps) | Pattern::Or(ps) => {
                return ps.iter().find_map(|sub| in_pattern(ws, mid, sub, offset));
            }
            _ => return None,
        };
        let on = |sp: Span| span_contains(&sp, offset);

        if path.segments.len() == 1 {
            let variant = path.segments.first()?;
            if on(variant.span) {
                if record && record_pattern_is_type(ws, mid, variant.span).unwrap_or(false) {
                    return None;
                }
                let enum_name = pattern_enum_name(ws, mid, variant.span).flatten();
                if record && enum_name.is_none() {
                    return None;
                }
                return variant_token(ws, enum_name, variant);
            }
            return subs
                .into_iter()
                .find_map(|sub| in_pattern(ws, mid, sub, offset));
        }

        let Some((enum_key, variant)) = path_variant(ws, mid, path) else {
            return subs
                .into_iter()
                .find_map(|sub| in_pattern(ws, mid, sub, offset));
        };
        let (enum_ident, variant_ident) = enum_path_names(path)?;
        if on(variant_ident.span) {
            return variant_token(ws, Some(enum_key), variant_ident);
        }
        let (es, _) = enum_ident.span.offset_range();
        let (vs, _) = variant_ident.span.offset_range();
        let span = enum_ident.span.with_offset_range((es, vs));
        if on(span) {
            enum_def_for_variant(ws, Some(enum_key), variant)?;
            return Some(Token {
                target: RefTarget::Enum(enum_key),
                span,
            });
        }
        subs.into_iter()
            .find_map(|sub| in_pattern(ws, mid, sub, offset))
    }
    ast.exprs.values().find_map(|ed| match &ed.kind {
        skull_parser::ExprKind::Match { arms, .. } => arms
            .iter()
            .find_map(|arm| in_pattern(ws, mid, &arm.pattern, offset)),
        _ => None,
    })
}

fn enum_def_for_variant(
    ws: &Workspace,
    enum_name: Option<Ustr>,
    variant: Ustr,
) -> Option<&hir::EnumDef> {
    if let Some(name) = enum_name {
        return enum_def_by_name(ws, name);
    }
    ws.modules.iter().find_map(|m| {
        m.enums
            .values()
            .find(|e| e.variants.iter().any(|v| v.name == variant))
    })
}

fn struct_def_by_name(ws: &Workspace, name: Ustr) -> Option<&hir::StructDef> {
    ws.modules
        .iter()
        .find_map(|m| m.struct_by_name.get(&name).map(|i| &m.structs[*i]))
}

/// `Variant` / `Variant($0)` / `Variant { $0 }`.
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

pub(crate) fn enum_variants_with_shape(
    ws: &Workspace,
    enum_name: &str,
) -> Vec<(String, VariantShape)> {
    enum_def_by_name(ws, Ustr::from(enum_name)).map_or_else(Vec::new, |e| {
        e.variants
            .iter()
            .map(|v| (v.name.to_string(), VariantShape::of(&v.payload)))
            .collect()
    })
}

fn newtype_def_by_name(ws: &Workspace, name: Ustr) -> Option<&NewtypeDef> {
    ws.modules
        .iter()
        .find_map(|m| m.newtypes_by_name.get(&name).map(|i| &m.newtypes[*i]))
}

pub(crate) struct UfcsMethod {
    pub name: String,
    pub signature: String,
    pub takes_args: bool,
    pub returns: Option<String>,
    /// Module-static; None when the method returns `()`.
    pub return_ty: Option<TyId>,
}

pub(crate) fn ufcs_methods_in_scope(
    ws: &Workspace,
    name: Ustr,
    recv_elem: Option<TyRef<'_>>,
    recv_is_iter: bool,
    frag: &bone_core::modules::FragmentScope,
) -> Vec<UfcsMethod> {
    let mut out = Vec::new();
    // Overloads collapse to one item, first wins, matching the general
    // path's per-name fn dedup.
    let mut seen: std::collections::HashSet<Ustr> = std::collections::HashSet::new();
    let callable_names =
        crate::modules_scope::spellings_by_key(ws, frag, bone_core::modules::DefKind::Callable);
    // Iterator receivers also get the `Iterator`-bounded combinators, whose
    // first param `it: I` is not a nominal match but is UFCS-legal.
    for m in ws.modules.iter() {
        for (fid, f) in m.functions.iter() {
            if source_ident(f.name).starts_with("__") {
                continue;
            }
            if !crate::modules_scope::callable_member_admitted(ws, frag, f.name, m.id, fid) {
                continue;
            }
            if let Some(p0) = f.param_decls.first() {
                let nominal_match = types::member_owner_name(ws, None, p0.ty) == Some(name);
                if nominal_match {
                    if let Some(want) = recv_elem {
                        if !vec_param_elem_matches(ws, p0.ty, want) {
                            continue;
                        }
                    }
                } else if !(recv_is_iter && fn_first_param_is_iter_bound(ws, f)) {
                    continue;
                }
                let Some(display_name) = callable_names
                    .get(&f.name)
                    .and_then(|names| names.first())
                    .copied()
                else {
                    continue;
                };
                if seen.insert(display_name) {
                    out.push(UfcsMethod {
                        name: display_name.to_string(),
                        signature: render_fn_signature(ws, display_name, f),
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

fn receiver_is_iterator(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
    recv: TyRef<'_>,
) -> bool {
    let Some((mut gamma, span)) = query_gamma_for(ws, mid, inf, offset, recv) else {
        return false;
    };
    inference::iterator_elem(&mut gamma, recv.ty, span).is_some()
}

fn fn_first_param_is_iter_bound(ws: &Workspace, f: &hir::Function) -> bool {
    let Some(p0) = f.param_decls.first() else {
        return false;
    };
    let hir::TypeExpr::Generic(pv, _) = types::ty_of(ws, None, p0.ty) else {
        return false;
    };
    f.generics.iter().zip(&f.generic_bounds).any(|(g, bound)| {
        bound.is_some()
            && matches!(types::ty_of(ws, None, *g), hir::TypeExpr::Generic(gv, _) if gv == pv)
    })
}

/// `p0_ty` is `Static` (param types are).
fn vec_param_elem_matches(ws: &Workspace, p0_ty: TyId, want: TyRef<'_>) -> bool {
    match types::array_elem(ws, None, p0_ty) {
        Some(elem) => {
            matches!(types::ty_of(ws, None, elem), hir::TypeExpr::Generic(..))
                || ws.types_equivalent(TyRef::of_static(elem), want)
        }
        None => false,
    }
}

pub(crate) fn struct_field_names_at(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Vec<Ustr> {
    let Some(ta) = bone_semantic::query::type_at(ws, mid, inf, offset) else {
        return Vec::new();
    };
    let Some(name) = ta
        .body
        .and_then(|bi| types::nominal_name(ws, Some(bi), ta.ty))
    else {
        return Vec::new();
    };
    match struct_def_by_name(ws, name) {
        Some(sdef) => sdef.fields.iter().map(|f| f.name).collect(),
        None => Vec::new(),
    }
}

pub(crate) fn struct_fields_by_name(ws: &Workspace, name: &str) -> Vec<(String, String, bool)> {
    match struct_def_by_name(ws, Ustr::from(name)) {
        Some(sdef) => sdef
            .fields
            .iter()
            .map(|f| (f.name.to_string(), ws.render(None, f.ty), f.has_default))
            .collect(),
        None => Vec::new(),
    }
}

/// The declared (static) TyId of struct `name`'s field `field`.
pub fn struct_field_ty(ws: &Workspace, name: &str, field: &str) -> Option<TyId> {
    let sdef = struct_def_by_name(ws, Ustr::from(name))?;
    sdef.fields
        .iter()
        .find(|f| f.name.as_str() == field)
        .map(|f| f.ty)
}

pub(crate) fn struct_field_fn_return_ty(ws: &Workspace, name: &str, field: &str) -> Option<TyId> {
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

pub(crate) fn variant_record_fields(
    ws: &Workspace,
    enum_name: &str,
    variant: &str,
) -> Vec<(String, String)> {
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

fn smallest_expr_covering(
    body: &Body,
    offset: usize,
    matches: impl Fn(&hir::Expr) -> bool,
) -> Option<Idx<hir::Expr>> {
    body.expr_arena
        .iter()
        .filter(|(_, expr)| matches(expr))
        .filter_map(|(e, _)| {
            let (start, end) = body.expr_spans.get(e)?.offset_range();
            (start <= offset && offset < end).then(|| (end - start, e))
        })
        .min_by_key(|(len, _)| *len)
        .map(|(_, e)| e)
}

pub(crate) fn match_arms_fill(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Option<(Span, Vec<String>)> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let bi = inf.per_fn.get(&owner)?;

    let e = smallest_expr_covering(body, offset, |expr| matches!(expr, hir::Expr::Match { .. }))?;
    let (enum_name, missing) = match_missing_variants(ws, body, bi, e)?;
    let arms_text: Vec<String> = missing
        .iter()
        .map(|v| format_variant_arm(enum_name, v))
        .collect();
    Some((*body.expr_spans.get(e)?, arms_text))
}

fn match_missing_variants<'a>(
    ws: &'a Workspace,
    body: &Body,
    bi: &BodyInference,
    e: Idx<hir::Expr>,
) -> Option<(Ustr, Vec<&'a hir::EnumVariantDef>)> {
    let hir::Expr::Match { scrutinee, arms } = &body.expr_arena[e] else {
        return None;
    };

    let scrut_ty = *bi.expr_types.get(*scrutinee)?;
    let enum_name = types::nominal_name(ws, Some(bi), scrut_ty)?;
    let edef = enum_def_by_name(ws, enum_name)?;

    // A guarded arm may not fire, so it covers nothing. An unguarded
    // wildcard or binding arm makes the match exhaustive.
    let mut covered: std::collections::HashSet<Ustr> = std::collections::HashSet::new();
    for arm_idx in *arms {
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

pub(crate) fn struct_fields_fill(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Option<(Span, Vec<String>)> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let bi = inf.per_fn.get(&owner)?;

    let e = smallest_expr_covering(body, offset, |expr| {
        matches!(expr, hir::Expr::StructLiteral { .. })
    })?;
    let hir::Expr::StructLiteral {
        name,
        fields,
        bases,
    } = &body.expr_arena[e]
    else {
        return None;
    };

    // A functional-update base (`{ ..base }`) supplies every unwritten field.
    if !bases.is_empty() {
        return None;
    }

    let sdef = struct_def_by_name(ws, *name)?;

    let mut present: std::collections::HashSet<Ustr> = std::collections::HashSet::new();
    for fidx in *fields {
        present.insert(body.struct_lit_field_arena[fidx].name);
    }

    // Shorthand only when the innermost binding of that name also matches
    // the field's type.
    let scope = locals_in_scope(ws, mid, inf, offset);
    let innermost_ty = |fname: Ustr| -> Option<TyId> {
        scope
            .iter()
            .filter(|(name, ..)| *name == fname)
            .max_by_key(|(.., end)| *end)
            .and_then(|(_, _, _, ty, _)| *ty)
    };

    let missing: Vec<String> = sdef
        .fields
        .iter()
        .filter(|f| !f.has_default && !present.contains(&f.name))
        .map(|f| {
            let matches = innermost_ty(f.name).is_some_and(|t| {
                ws.types_equivalent(TyRef::new(Some(bi), t), TyRef::of_static(f.ty))
            });
            if matches {
                f.name.to_string()
            } else {
                format!("{}: ?", f.name)
            }
        })
        .collect();

    if missing.is_empty() {
        return None;
    }
    Some((*body.expr_spans.get(e)?, missing))
}

/// The `let` to delete, its initializer and every use. `parenthesize` keeps
/// the original precedence for a non-atomic initializer.
pub struct InlineLocalSites {
    pub var: Ustr,
    pub let_span: Span,
    pub init_span: Span,
    pub uses: Vec<Span>,
    pub parenthesize: bool,
}

/// Find a let binding and the uses where its initializer can be safely inlined.
pub fn inline_local(ws: &Workspace, mid: ModuleId, offset: usize) -> Option<InlineLocalSites> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let bi = ws.inference(mid)?.per_fn.get(&owner)?;
    let b = binding_at(body, offset)?;

    // Params, `for` vars and destructuring lets have no single `Statement::Let`.
    let (let_idx, value) = body.statement_arena.iter().find_map(|(i, s)| match s {
        hir::Statement::Let { pattern, value, .. }
            if body.pattern_single_bind(*pattern) == Some(b) =>
        {
            Some((i, *value))
        }
        _ => None,
    })?;
    let let_span = *body.statement_spans.get(let_idx)?;
    let init_span = *body.expr_spans.get(value)?;
    // Inline-local edits one document; macro defaults can retain another file's spans.
    if let_span.file != module.file || init_span.file != module.file {
        return None;
    }
    let parenthesize = init_needs_parens(&body.expr_arena[value]);

    let mut uses = Vec::new();
    for (e, expr) in body.expr_arena.iter() {
        if matches!(expr, hir::Expr::Var(x) if *x == b) {
            if let Some(s) = body.expr_spans.get(e) {
                if s.file != module.file {
                    return None;
                }
                uses.push(*s);
            }
        }
    }

    if !init_operators_are_pure(ws, bi, body, value) {
        return None;
    }
    if !init_is_pure(ws, bi, body, value)
        && (uses.len() != 1 || uses_rerun_outside_let(body, let_span, &uses))
    {
        return None;
    }

    Some(InlineLocalSites {
        var: body.binding_arena[b].name,
        let_span,
        init_span,
        uses,
        parenthesize,
    })
}

/// Whether evaluating `e` more than once, or not at all, is unobservable.
fn init_is_pure(ws: &Workspace, bi: &BodyInference, body: &hir::Body, e: Idx<hir::Expr>) -> bool {
    match &body.expr_arena[e] {
        hir::Expr::Unit
        | hir::Expr::Null
        | hir::Expr::Int(_)
        | hir::Expr::Float(_)
        | hir::Expr::Bool(_)
        | hir::Expr::String(_)
        | hir::Expr::Var(_)
        | hir::Expr::Global(_)
        | hir::Expr::SiblingRef(_) => true,
        hir::Expr::FieldAccess { base, .. }
        | hir::Expr::SafeNav { base, .. }
        | hir::Expr::TupleIndex { tuple: base, .. }
        | hir::Expr::BitNot(base) => init_is_pure(ws, bi, body, *base),
        hir::Expr::UnaryMinus(base, _) | hir::Expr::Not(base, _) => {
            init_operator_is_pure(ws, bi, body, e) == Some(true)
                && init_is_pure(ws, bi, body, *base)
        }
        hir::Expr::BinOp { lhs, rhs, .. } => {
            init_is_pure(ws, bi, body, *lhs) && init_is_pure(ws, bi, body, *rhs)
        }
        // Binary arithmetic lowers to a call of its `__op_*` operator function.
        hir::Expr::Call { args, .. } => {
            let mut args = *args;
            init_operator_is_pure(ws, bi, body, e) == Some(true)
                && args.all(|arg| init_is_pure(ws, bi, body, arg))
        }
        _ => false,
    }
}

fn init_operator_is_pure(
    ws: &Workspace,
    bi: &BodyInference,
    body: &hir::Body,
    e: Idx<hir::Expr>,
) -> Option<bool> {
    match &body.expr_arena[e] {
        hir::Expr::UnaryMinus(..) | hir::Expr::Not(..)
            if !bi.resolved_overload_calls.contains_key(&e) =>
        {
            return Some(
                bi.expr_types
                    .get(e)
                    .and_then(|&ty| ws.resolve_ty(Some(bi), ty))
                    .is_some_and(|(_, ty)| match ty {
                        hir::TypeExpr::Builtin(
                            hir::BuiltinTy::Int | hir::BuiltinTy::Float | hir::BuiltinTy::Bool,
                        ) => true,
                        hir::TypeExpr::App {
                            ctor: hir::TypeCtor::Nominal(name),
                            ..
                        } => ws.newtypes_by_name.contains_key(&name),
                        _ => false,
                    }),
            );
        }
        hir::Expr::UnaryMinus(..) | hir::Expr::Not(..) => {}
        hir::Expr::Call {
            callee: hir::Callee::Unresolved { name, .. },
            ..
        } => {
            let name = name.as_str().rsplit("::").next().unwrap_or_default();
            if hir::operator_entry(name).is_none() && hir::unary_operator_entry(name).is_none() {
                return None;
            }
        }
        _ => return None,
    }
    Some(
        bi.resolved_overload_calls
            .get(&e)
            .is_some_and(|&(mid, fid)| {
                ws.module(mid).functions[fid]
                    .annotations
                    .iter()
                    .any(|a| a.is_builtin("pure"))
                    && ws.effects().of(mid, fid).is_none()
            }),
    )
}

fn init_operators_are_pure(
    ws: &Workspace,
    bi: &BodyInference,
    body: &hir::Body,
    e: Idx<hir::Expr>,
) -> bool {
    use bone_core::hir_walk::HirChild;

    let mut pure = init_operator_is_pure(ws, bi, body, e) != Some(false);
    body.walk_descendants(HirChild::Expr(e), &mut |node| {
        if let HirChild::Expr(e) = node {
            pure &= init_operator_is_pure(ws, bi, body, e) != Some(false);
        }
        pure
    });
    pure
}

/// A region that also holds the `let` re-runs the initializer too.
fn uses_rerun_outside_let(body: &hir::Body, let_span: Span, uses: &[Span]) -> bool {
    let mut regions: Vec<Span> = Vec::new();
    for (i, statement) in body.statement_arena.iter() {
        match statement {
            // The iterable is evaluated once; only the body repeats.
            hir::Statement::For {
                body: loop_body, ..
            } => {
                regions.extend(body.expr_spans.get(*loop_body).copied());
            }
            hir::Statement::While { .. } | hir::Statement::Fn { .. } | hir::Statement::Defer(_) => {
                regions.extend(body.statement_spans.get(i).copied());
            }
            _ => {}
        }
    }
    for (e, expr) in body.expr_arena.iter() {
        if matches!(expr, hir::Expr::Lambda { .. }) {
            regions.extend(body.expr_spans.get(e).copied());
        }
    }
    regions.iter().any(|&region| {
        !region.contains(let_span) && uses.iter().any(|&use_span| region.contains(use_span))
    })
}

#[cfg(test)]
mod rerun_region_tests {
    use super::*;

    #[test]
    fn rerun_regions_only_contain_spans_from_their_own_file() {
        let span = |file: &str, start, end| {
            Span::new(Ustr::from(file), (0, 0), (start, end), (start, end))
        };
        let mut body = hir::Body::default();
        let unit = body.expr_arena.alloc(hir::Expr::Unit);
        body.alloc_statement(hir::Statement::Defer(unit), span("region.rock", 10, 30));

        assert!(!uses_rerun_outside_let(
            &body,
            span("binding.rock", 0, 5),
            &[span("binding.rock", 20, 21)],
        ));
        assert!(uses_rerun_outside_let(
            &body,
            span("binding.rock", 15, 16),
            &[span("region.rock", 20, 21)],
        ));
        assert!(!uses_rerun_outside_let(
            &body,
            span("region.rock", 15, 16),
            &[span("region.rock", 20, 21)],
        ));
    }
}

fn init_needs_parens(e: &hir::Expr) -> bool {
    !matches!(
        e,
        hir::Expr::Unit
            | hir::Expr::Null
            | hir::Expr::Error
            | hir::Expr::Hole(_)
            | hir::Expr::Int(_)
            | hir::Expr::Float(_)
            | hir::Expr::Bool(_)
            | hir::Expr::String(_)
            | hir::Expr::Var(_)
            | hir::Expr::Tuple(_)
            | hir::Expr::InterpolatedString(_)
            | hir::Expr::Index { .. }
            | hir::Expr::Call { .. }
            | hir::Expr::StructLiteral { .. }
            | hir::Expr::FieldAccess { .. }
            | hir::Expr::SafeNav { .. }
            | hir::Expr::SafeNavCall { .. }
            | hir::Expr::TupleIndex { .. }
            | hir::Expr::EnumVariant { .. }
    )
}

/// Data for a "Generate function" stub, inserted just past the enclosing
/// function.
pub struct GenerateFn {
    pub name: String,
    pub params: Vec<(String, String)>,
    pub ret: Option<String>,
    pub insert_at: usize,
}

/// Stub data for an unresolved call `name(...)` at `offset`. None when a
/// function of that name exists (the call is then a type or arity error).
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

    let mut best: Option<(usize, Idx<hir::Expr>)> = None;
    for (e, expr) in body.expr_arena.iter() {
        let hir::Expr::Call {
            callee: hir::Callee::Unresolved { .. },
            ..
        } = expr
        else {
            continue;
        };
        if bi.resolved_overload_calls.contains_key(&e) {
            continue;
        }
        let Some(sp) = body.expr_spans.get(e) else {
            continue;
        };
        let (s, en) = sp.offset_range();
        if s <= offset && offset < en {
            let len = en - s;
            if best.map_or(true, |(bl, _)| len < bl) {
                best = Some((len, e));
            }
        }
    }
    let (_, e) = best?;
    let hir::Expr::Call {
        callee: hir::Callee::Unresolved { name, .. },
        args,
        ..
    } = &body.expr_arena[e]
    else {
        return None;
    };

    // An existing function of this name makes the call a type error, not a missing function.
    if function_name_exists(ws, *name) {
        return None;
    }

    let params: Vec<(String, String)> = (*args)
        .enumerate()
        .map(|(i, arg)| {
            (
                arg_param_name(body, arg, i),
                arg_param_type(ws, bi, body, arg),
            )
        })
        .collect();

    // bone does not type an unresolved call's result node, so the use-site
    // `let` annotation is the only signal for a typed `let` to keep checking.
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
    let insert_at = module.functions[fid].span.offset_range().1;

    Some(GenerateFn {
        name: name.to_string(),
        params,
        ret,
        insert_at,
    })
}

fn arg_param_name(body: &Body, arg: Idx<hir::Expr>, i: usize) -> String {
    match &body.expr_arena[arg] {
        hir::Expr::Var(b) => body.binding_arena[*b].name.to_string(),
        hir::Expr::Global(n) | hir::Expr::SiblingRef(n) => source_ident(*n).to_string(),
        _ => format!("arg{i}"),
    }
}

/// bone does not type an unresolved call's argument nodes. `any` is the
/// last fallback because `?` is a hard `Infer` error here.
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

fn binding_for_value(body: &Body, value: Idx<hir::Expr>) -> Option<Idx<hir::BindingInfo>> {
    for (_, st) in body.statement_arena.iter() {
        if let hir::Statement::Let {
            pattern, value: v, ..
        } = st
            && *v == value
        {
            let pat = match body.pattern_arena[*pattern] {
                hir::Pattern::Newtype { sub, .. } => sub,
                _ => *pattern,
            };
            if let hir::Pattern::Bind(b) = body.pattern_arena[pat] {
                return Some(b);
            }
        }
    }
    None
}

fn function_name_exists(ws: &Workspace, name: Ustr) -> bool {
    ws.modules.iter().any(|m| {
        m.functions_by_name
            .get(&name)
            .is_some_and(|l| !l.is_empty())
    })
}

/// Concrete types only: `-> ?` or `x: ?` is a hard bone error
/// (`unsupported type expr: Infer`).
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
            for alt in *alternatives {
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
            format!(
                "{}::{}({})",
                enum_name,
                v.name,
                vec!["_"; tys.len()].join(", ")
            )
        }
        hir::VariantPayload::Record(_) => format!("{}::{} {{ .. }}", enum_name, v.name),
    };
    format!("{pat} => {{}}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymCat {
    Function,
    /// Expanded before lowering, so sourced from the Session's
    /// `UserFile.macros`, never `ws`.
    Macro,
    Struct,
    Enum,
    Variant,
    Newtype,
    Global,
    Local,
    Param,
    Keyword,
}

pub(crate) struct CompletionEntry {
    pub name: Ustr,
    pub symbol: CompletionSymbol,
    pub detail: Option<String>,
    pub test_scoped: bool,
    pub def_file: Ustr,
}

pub(crate) enum CompletionSymbol {
    Function {
        key: Ustr,
        takes_params: bool,
    },
    Macro {
        takes_params: bool,
    },
    Variable {
        kind: SymCat,
        ty: Option<TyId>,
        rec: u8,
    },
    Variant {
        enum_key: Ustr,
        shape: VariantShape,
    },
    Type(SymCat),
    Keyword,
}

impl CompletionEntry {
    pub(crate) fn new(name: Ustr, symbol: CompletionSymbol, def_file: Ustr) -> Self {
        Self {
            name,
            symbol,
            detail: None,
            test_scoped: false,
            def_file,
        }
    }

    pub(crate) fn kind(&self) -> SymCat {
        match self.symbol {
            CompletionSymbol::Function { .. } => SymCat::Function,
            CompletionSymbol::Macro { .. } => SymCat::Macro,
            CompletionSymbol::Variable { kind, .. } | CompletionSymbol::Type(kind) => kind,
            CompletionSymbol::Variant { .. } => SymCat::Variant,
            CompletionSymbol::Keyword => SymCat::Keyword,
        }
    }

    pub(crate) fn key(&self) -> Ustr {
        match self.symbol {
            CompletionSymbol::Function { key, .. } => key,
            _ => self.name,
        }
    }

    pub(crate) fn rec(&self) -> u8 {
        match self.symbol {
            CompletionSymbol::Variable { rec, .. } => rec,
            _ => 0,
        }
    }
}

/// Contains no workspace-local IDs.
#[derive(Clone, Debug)]
pub(crate) struct VisibleBinding {
    name: Ustr,
    key: Ustr,
    kind: VisibleBindingKind,
    def_file: Ustr,
    file_is_test: bool,
}

#[derive(Clone, Debug)]
enum VisibleBindingKind {
    Function {
        takes_params: bool,
        multiple_overloads: bool,
    },
    Struct,
    Enum(Vec<VisibleEnumVariant>),
    Newtype,
    Global,
}

#[derive(Clone, Debug)]
struct VisibleEnumVariant {
    suffix: Ustr,
    qualified: Ustr,
    shape: VariantShape,
}

pub struct SymbolEntry {
    pub name: String,
    pub kind: SymCat,
    pub span: Span,
    /// Both kinds are `SymCat::Global`, but editors show them as different
    /// symbol kinds.
    pub is_const: bool,
}

#[derive(Clone)]
pub struct SignatureEntry {
    pub label: String,
    pub params: Vec<String>,
}

fn first_callable_in_scope(
    ws: &Workspace,
    frag: &bone_core::modules::FragmentScope,
    key: Ustr,
) -> Option<(ModuleId, Idx<Function>)> {
    ws.functions_by_name.get(&key).and_then(|functions| {
        functions.iter().copied().find(|&(mid, fid)| {
            crate::modules_scope::callable_member_admitted(ws, frag, key, mid, fid)
        })
    })
}

pub(crate) fn resolve_visible_binding(
    ws: &Workspace,
    frag: &bone_core::modules::FragmentScope,
    name: ustr::Ustr,
    key: ustr::Ustr,
    global_def_file: Option<Ustr>,
    test_path_cache: &mut FxHashMap<Ustr, bool>,
) -> Option<VisibleBinding> {
    let function_family = ws.functions_by_name.get(&key);
    let (kind, def_file) = if let Some((fmid, fid)) = first_callable_in_scope(ws, frag, key) {
        let module = ws.module(fmid);
        (
            VisibleBindingKind::Function {
                takes_params: !module.functions[fid].param_decls.is_empty(),
                multiple_overloads: function_family.is_some_and(|functions| functions.len() > 1),
            },
            module.file,
        )
    } else if let Some(sid) = ws.struct_by_name.get(&key) {
        (VisibleBindingKind::Struct, ws.module(sid.module).file)
    } else if let Some(eid) = ws.enums_by_name.get(&key) {
        let module = ws.module(eid.module);
        let variants = module.enums[eid.def]
            .variants
            .iter()
            .map(|variant| {
                let qualified = format!("{name}::{}", variant.name);
                VisibleEnumVariant {
                    suffix: variant.name,
                    qualified: Ustr::from(qualified.as_str()),
                    shape: VariantShape::of(&variant.payload),
                }
            })
            .collect();
        (VisibleBindingKind::Enum(variants), module.file)
    } else if let Some(nid) = ws.newtypes_by_name.get(&key) {
        (VisibleBindingKind::Newtype, ws.module(nid.module).file)
    } else if ws.globals_by_name.contains_key(&key) {
        let def_file = global_def_file.unwrap_or_else(|| {
            ws.modules
                .iter()
                .find(|module| module.global_by_name.contains_key(&key))
                .map_or_else(|| Ustr::from("<builtin>"), |module| module.file)
        });
        (VisibleBindingKind::Global, def_file)
    } else {
        return None;
    };
    let file_is_test = *test_path_cache
        .entry(def_file)
        .or_insert_with(|| is_test_path(def_file.as_str()));
    Some(VisibleBinding {
        name,
        key,
        kind,
        def_file,
        file_is_test,
    })
}

/// The global type is read from the current workspace so the cache never
/// retains a `TyId` across updates.
fn emit_visible_binding_entries(
    ws: &Workspace,
    binding: &VisibleBinding,
    frag: &bone_core::modules::FragmentScope,
    expr_position: bool,
    pfx: &[u8],
    show_private: bool,
    test_path_cache: &mut FxHashMap<Ustr, bool>,
    out: &mut Vec<CompletionEntry>,
) {
    let visible = |candidate: &str| !candidate.starts_with('_') || show_private;
    let matches = |candidate: &str| {
        visible(candidate) && crate::completion::match_policy::admit(candidate.as_bytes(), pfx)
    };
    let mut def_file = binding.def_file;
    let mut file_is_test = binding.file_is_test;
    let mut current_takes_params = None;
    if let VisibleBindingKind::Function {
        takes_params,
        multiple_overloads,
        ..
    } = &binding.kind
    {
        current_takes_params = Some(*takes_params);
        if *multiple_overloads && expr_position && matches(binding.name.as_str()) {
            let Some((mid, fid)) = first_callable_in_scope(ws, frag, binding.key) else {
                return;
            };
            def_file = ws.module(mid).file;
            file_is_test = *test_path_cache
                .entry(def_file)
                .or_insert_with(|| is_test_path(def_file.as_str()));
            current_takes_params = Some(!ws.module(mid).functions[fid].param_decls.is_empty());
        }
    }
    let entry = |name: Ustr, symbol| CompletionEntry {
        test_scoped: file_is_test || name.as_str().starts_with("test_"),
        ..CompletionEntry::new(name, symbol, def_file)
    };
    match &binding.kind {
        VisibleBindingKind::Function { takes_params, .. } => {
            if expr_position && matches(binding.name.as_str()) {
                out.push(entry(
                    binding.name,
                    CompletionSymbol::Function {
                        key: binding.key,
                        takes_params: current_takes_params.unwrap_or(*takes_params),
                    },
                ));
            }
        }
        VisibleBindingKind::Struct => {
            if matches(binding.name.as_str()) {
                out.push(entry(binding.name, CompletionSymbol::Type(SymCat::Struct)));
            }
        }
        VisibleBindingKind::Enum(variants) => {
            if expr_position && !binding.def_file.as_str().starts_with('<') {
                for variant in variants {
                    let suffix = variant.suffix.as_str();
                    let qualified = variant.qualified.as_str();
                    let hit = pfx.is_empty()
                        || crate::completion::match_policy::admit(suffix.as_bytes(), pfx)
                        || crate::completion::match_policy::admit(qualified.as_bytes(), pfx);
                    if hit && visible(suffix) {
                        out.push(CompletionEntry {
                            detail: Some(binding.name.to_string()),
                            ..entry(
                                variant.qualified,
                                CompletionSymbol::Variant {
                                    enum_key: binding.key,
                                    shape: variant.shape,
                                },
                            )
                        });
                    }
                }
            } else if matches(binding.name.as_str()) {
                out.push(entry(binding.name, CompletionSymbol::Type(SymCat::Enum)));
            }
        }
        VisibleBindingKind::Newtype => {
            if matches(binding.name.as_str()) {
                out.push(entry(binding.name, CompletionSymbol::Type(SymCat::Newtype)));
            }
        }
        VisibleBindingKind::Global => {
            if expr_position && matches(binding.name.as_str()) {
                if let Some(&ty) = ws.globals_by_name.get(&binding.key) {
                    out.push(entry(
                        binding.name,
                        CompletionSymbol::Variable {
                            kind: SymCat::Global,
                            ty: Some(ty),
                            rec: 0,
                        },
                    ));
                }
            }
        }
    }
}

pub(crate) fn key_entries_in_scope(
    ws: &Workspace,
    frag: &bone_core::modules::FragmentScope,
    name: ustr::Ustr,
    key: ustr::Ustr,
    expr_position: bool,
    pfx: &[u8],
    show_private: bool,
    out: &mut Vec<CompletionEntry>,
    test_path_cache: &mut FxHashMap<Ustr, bool>,
) {
    if let Some(binding) = resolve_visible_binding(ws, frag, name, key, None, test_path_cache) {
        emit_visible_binding_entries(
            ws,
            &binding,
            frag,
            expr_position,
            pfx,
            show_private,
            test_path_cache,
            out,
        );
    }
}

pub(crate) fn completion_with_visible_bindings(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
    prefix: &str,
    expr_position: bool,
    visible_bindings: &[VisibleBinding],
) -> Vec<CompletionEntry> {
    let mut seen: FxHashSet<ustr::Ustr> = FxHashSet::default();
    let mut out = Vec::new();

    let pfx = prefix.as_bytes();
    // `_`-prefixed symbols are soft-private: hidden unless the prefix starts
    // with `_`.
    let show_private = prefix.starts_with('_');
    let matches = |name: &str| -> bool {
        if name.starts_with('_') && !show_private {
            return false;
        }
        crate::completion::match_policy::admit(name.as_bytes(), pfx)
    };

    // Locals shadow globals: emit them first so `seen` suppresses a
    // same-named global.
    let req_file = ws
        .modules
        .get(mid.index())
        .map_or_else(ustr::Ustr::default, |m| m.file);
    let Some(frag) = crate::modules_scope::fragment(ws, req_file) else {
        return Vec::new();
    };
    if expr_position {
        let locals = locals_in_scope(ws, mid, inf, offset);
        // Params carry the earliest spans, so an inner `let` beats an enclosing
        // param on ties.
        let mut by_decl: Vec<usize> = (0..locals.len()).collect();
        by_decl.sort_by_key(|&i| std::cmp::Reverse(locals[i].4));
        let mut rec_of = vec![0u8; locals.len()];
        for (rank, &i) in by_decl.iter().enumerate() {
            rec_of[i] = rank.min(9) as u8;
        }
        for (i, (name, kind, ty, ty_id, _decl)) in locals.into_iter().enumerate() {
            if !matches(name.as_str()) {
                continue;
            }
            if seen.insert(name) {
                let kind = if kind == BindingKind::Param {
                    SymCat::Param
                } else {
                    SymCat::Local
                };
                out.push(CompletionEntry {
                    detail: (!ty.is_empty()).then_some(ty),
                    ..CompletionEntry::new(
                        name,
                        CompletionSymbol::Variable {
                            kind,
                            ty: ty_id,
                            rec: rec_of[i],
                        },
                        req_file,
                    )
                });
            }
        }
    }

    seen.reserve(visible_bindings.len());
    out.reserve(visible_bindings.len());
    let mut scratch = Vec::new();
    let mut test_path_cache = FxHashMap::default();
    for binding in visible_bindings {
        let name = binding.name.as_str();
        // An enum can match through its variants even when its own name does not match.
        if name.starts_with('_') && !show_private
            || !(matches(name)
                || (expr_position && name.starts_with(|c: char| c.is_ascii_uppercase())))
        {
            continue;
        }
        emit_visible_binding_entries(
            ws,
            binding,
            frag,
            expr_position,
            pfx,
            show_private,
            &mut test_path_cache,
            &mut scratch,
        );
        out.extend(scratch.drain(..).filter(|entry| seen.insert(entry.name)));
    }
    // Vec and native buffers already come from the struct declarations.
    if !expr_position {
        for name in BUILTIN_TYPE_NAMES {
            let n = ustr::Ustr::from(name);
            if matches(name) && seen.insert(n) {
                out.push(CompletionEntry::new(
                    n,
                    CompletionSymbol::Type(SymCat::Struct),
                    Ustr::from("<builtin>"),
                ));
            }
        }
    }

    if expr_position && enclosing_is_coroutine(ws, mid, offset) {
        for kw in ["yield", "await"] {
            let n = ustr::Ustr::from(kw);
            if matches(kw) && seen.insert(n) {
                out.push(CompletionEntry::new(
                    n,
                    CompletionSymbol::Keyword,
                    Ustr::from("<builtin>"),
                ));
            }
        }
    }

    out
}

/// Builtin type names with no workspace name-index entry.
const BUILTIN_TYPE_NAMES: &[&str] = &[
    "Coroutine",
    "Map",
    "int",
    "uint",
    "float",
    "f32",
    "f64",
    "string",
    "bool",
    "any",
];

fn enclosing_is_coroutine(ws: &Workspace, mid: ModuleId, offset: usize) -> bool {
    let Some(module) = ws.modules.get(mid.index()) else {
        return false;
    };
    matches!(
        module.body_at(offset),
        Some(hir::BodyOwner::Function(fid)) if module.functions[fid].is_coroutine
    )
}

pub(crate) fn rendered_params(ws: &Workspace, params: &[hir::ParamDecl]) -> Vec<String> {
    params
        .iter()
        .map(|p| format!("{}: {}", p.name, ws.render(None, p.ty)))
        .collect()
}

pub(crate) fn rendered_macro_params(md: &bone_incremental::session::MacroDef) -> Vec<String> {
    md.params
        .iter()
        .map(|p| {
            let variadic = if p.is_variadic { "..." } else { "" };
            format!("{}: {}{}", p.name, variadic, p.ty)
        })
        .collect()
}

pub(crate) fn render_fn_signature(
    ws: &Workspace,
    name: ustr::Ustr,
    f: &bone_semantic::hir::Function,
) -> String {
    let params = rendered_params(ws, &f.param_decls);
    format_fn_signature(ws, name, &params, f.return_ty)
}

fn format_fn_signature(ws: &Workspace, name: Ustr, params: &[String], ret: Option<TyId>) -> String {
    let ret = ret
        .map(|t| format!(" -> {}", ws.render(None, t)))
        .unwrap_or_default();
    format!(
        "fn {}({}){}",
        display_name(name.as_str()),
        params.join(", "),
        ret
    )
}

/// The entry carries only a canonical key; workspace-local IDs are looked
/// up after selection.
pub(crate) fn render_deferred_signature(
    ws: &Workspace,
    fragment_file: Ustr,
    key: Ustr,
    display_name: Ustr,
) -> Option<String> {
    let frag = crate::modules_scope::fragment(ws, fragment_file)?;
    let (mid, fid) = first_callable_in_scope(ws, frag, key)?;
    Some(render_fn_signature(
        ws,
        display_name,
        &ws.module(mid).functions[fid],
    ))
}

/// File path -> source text (the LSP's per-project `texts`).
pub(crate) type SourceMap = std::collections::HashMap<String, String>;

/// An optional param whose default source is unreachable renders
/// `name: T = ..`.
pub(crate) fn render_fn_signature_with_defaults(
    ws: &Workspace,
    fmid: ModuleId,
    fid: Idx<Function>,
    sources: Option<&SourceMap>,
) -> String {
    let module = &ws.modules[fmid.index()];
    let f = &module.functions[fid];
    let default_at = |i: usize| {
        let body = module.bodies.get(&hir::BodyOwner::Function(fid))?;
        let expr = (*body.param_defaults.get(i)?)?;
        span_text(
            sources?.get(module.file.as_str())?,
            body.expr_spans.get(expr)?,
        )
        .map(str::trim)
    };
    let mut params = rendered_params(ws, &f.param_decls);
    for (i, (param, rendered)) in f.param_decls.iter().zip(&mut params).enumerate() {
        if param.has_default {
            rendered.push_str(" = ");
            rendered.push_str(default_at(i).unwrap_or(".."));
        }
    }
    format_fn_signature(ws, f.name, &params, f.return_ty)
}

pub(crate) fn fn_ctags_signatures(
    ws: &Workspace,
    mid: ModuleId,
) -> std::collections::HashMap<usize, (String, Option<String>)> {
    let mut out = std::collections::HashMap::new();
    let Some(m) = ws.modules.get(mid.index()) else {
        return out;
    };
    for (_, f) in m.functions.iter() {
        let params = rendered_params(ws, &f.param_decls);
        let returns = f.return_ty.map(|t| ws.render(None, t));
        out.insert(
            f.span.offset_range().0,
            (format!("({})", params.join(", ")), returns),
        );
    }
    out
}

/// List top-level declarations in arena order.
pub fn document_symbols(ws: &Workspace, mid: ModuleId) -> Vec<SymbolEntry> {
    let Some(m) = ws.modules.get(mid.index()) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut push = |name: ustr::Ustr, kind: SymCat, span: Span, is_const: bool| {
        out.push(SymbolEntry {
            name: display_name(name.as_str()).to_string(),
            kind,
            span,
            is_const,
        });
    };

    for (_, f) in m.functions.iter() {
        push(f.name, SymCat::Function, f.span, false);
    }
    for (_, s) in m.structs.iter() {
        push(s.name, SymCat::Struct, s.span, false);
    }
    for (_, e) in m.enums.iter() {
        push(e.name, SymCat::Enum, e.span, false);
    }
    for (_, n) in m.newtypes.iter() {
        push(n.name, SymCat::Newtype, n.span, false);
    }
    for (_, g) in m.globals.iter() {
        push(g.name, SymCat::Global, g.span, g.is_const);
    }

    out
}

/// Prefer the selected overload, then fall back to by-name signatures or a constructor.
pub fn signature_help(
    ws: &Workspace,
    mid: ModuleId,
    inf: Option<&InferenceResult>,
    offset: usize,
    sources: Option<&SourceMap>,
) -> Option<(Vec<SignatureEntry>, Span)> {
    let module = ws.modules.get(mid.index())?;
    let frag = crate::modules_scope::fragment(ws, module.file)?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;

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
        hir::Expr::Call { callee, args, .. } => {
            if let Some((cmid, fid)) = inf
                .and_then(|inf| inf.per_fn.get(&owner))
                .and_then(|bi| bi.resolved_overload_calls.get(&e))
            {
                return Some((vec![fn_sig_entry(ws, *cmid, *fid, sources)], call_span));
            }
            let hir::Callee::Unresolved { name, .. } = callee else {
                return None;
            };
            // Drop overloads whose first param is a different concrete nominal
            // (`rb.push()` must not offer `Vec.push`).
            let recv_nominal = inf.and_then(|inf| inf.per_fn.get(&owner)).and_then(|bi| {
                let first = args.clone().next()?;
                types::member_owner_name(ws, Some(bi), *bi.expr_types.get(first)?)
            });
            let sigs = functions_named(ws, *name, sources, recv_nominal, frag);
            (!sigs.is_empty()).then_some((sigs, call_span))
        }
        hir::Expr::StructLiteral { name, .. } => {
            Some((vec![struct_ctor_sig(ws, *name)?], call_span))
        }
        _ => None,
    }
}

/// Per-param labels stay bare `name: T` so the client's active-param
/// highlight finds each as a substring of the label.
fn fn_sig_entry(
    ws: &Workspace,
    fmid: ModuleId,
    fid: Idx<Function>,
    sources: Option<&SourceMap>,
) -> SignatureEntry {
    let f = &ws.modules[fmid.index()].functions[fid];
    let params = rendered_params(ws, &f.param_decls);
    SignatureEntry {
        label: render_fn_signature_with_defaults(ws, fmid, fid, sources),
        params,
    }
}

/// If the `recv_nominal` filter drops every overload, the unfiltered list
/// is returned.
fn functions_named(
    ws: &Workspace,
    name: Ustr,
    sources: Option<&SourceMap>,
    recv_nominal: Option<Ustr>,
    frag: &bone_core::modules::FragmentScope,
) -> Vec<SignatureEntry> {
    let mut all = Vec::new();
    let mut matched = Vec::new();
    for (fmid, fid, f) in crate::modules_scope::admitted_overloads(ws, frag, name) {
        let entry = fn_sig_entry(ws, fmid, fid, sources);
        if let Some(recv) = recv_nominal {
            let keep = f
                .param_decls
                .first()
                .and_then(|p| types::member_owner_name(ws, None, p.ty))
                .map_or(true, |pn| pn == recv);
            if keep {
                matched.push(entry.clone());
            }
        }
        all.push(entry);
    }
    if recv_nominal.is_some() && !matched.is_empty() {
        matched
    } else {
        all
    }
}

fn struct_ctor_sig(ws: &Workspace, name: Ustr) -> Option<SignatureEntry> {
    let params: Vec<String> = struct_def_by_name(ws, name)?
        .fields
        .iter()
        .map(|f| format!("{}: {}", f.name, ws.render(None, f.ty)))
        .collect();
    let label = format!(
        "{} {{ {} }}",
        display_name(name.as_str()),
        params.join(", ")
    );
    Some(SignatureEntry { label, params })
}

/// Which kind of `?` hole the cursor sits on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HoleKind {
    /// `?` in type position, e.g. `let x: ? = expr`.
    Type,
    /// `?` in value position, e.g. `let x = ?`.
    Expr,
}

/// The hole kind, its solved type when available, and nearby bindings.
pub struct HoleInfo {
    pub kind: HoleKind,
    pub ty: Option<String>,
    pub in_scope: Vec<(String, String)>,
    /// Source span of the `?` itself, so the hover handler can range its result.
    pub span: Span,
}

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
        .map(|(name, _kind, ty, _ty_id, _decl)| (name.to_string(), ty))
        .collect();
    Some(HoleInfo {
        kind,
        ty,
        in_scope,
        span,
    })
}

pub(crate) fn all_holes(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    ast: &Ast,
) -> Vec<(Span, HoleKind, Option<String>)> {
    let mut out = Vec::new();
    for (_, t) in ast.types.iter() {
        if matches!(t.kind, skull_parser::TypeExprKind::Hole(..)) {
            let ty = type_at_offset(ws, mid, inf, t.span.offset_range().0);
            out.push((t.span, HoleKind::Type, ty));
        }
    }
    for (_, e) in ast.exprs.iter() {
        if matches!(e.kind, skull_parser::ExprKind::Hole(..)) {
            let ty = type_at_offset(ws, mid, inf, e.span.offset_range().0);
            out.push((e.span, HoleKind::Expr, ty));
        }
    }
    out
}

fn hole_kind_at(ast: &Ast, offset: usize) -> Option<(HoleKind, Span)> {
    for (_, t) in ast.types.iter() {
        if matches!(t.kind, skull_parser::TypeExprKind::Hole(..)) && span_contains(&t.span, offset)
        {
            return Some((HoleKind::Type, t.span));
        }
    }
    for (_, e) in ast.exprs.iter() {
        if matches!(e.kind, skull_parser::ExprKind::Hole(..)) && span_contains(&e.span, offset) {
            return Some((HoleKind::Expr, e.span));
        }
    }
    None
}

fn type_at_offset(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Option<String> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let bi = inf.per_fn.get(&owner)?;
    let node = node_at(body, offset)?;
    let ty = match node {
        NodeRef::LocalTy(idx) => TyId::Local(idx),
        NodeRef::StaticTy { mid, idx } => TyId::Static(mid, idx),
        NodeRef::Expr(e) => *bi.expr_types.get(e)?,
        _ => return None,
    };
    Some(ws.render(Some(bi), ty))
}

/// Delimiter-only word boundary, as in `fuzzy_score`; no camelCase, unlike
/// `clean_match`.
pub(crate) fn fz_boundary(cand: &[u8], j: usize) -> bool {
    j == 0 || matches!(cand[j - 1], b'_' | b'.' | b'(' | b')' | b':' | b' ')
}

/// A clean subsequence starts at word boundaries or continues an existing run.
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
