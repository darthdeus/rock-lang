use bone_core::modules::display_name;
use bone_core::type_equiv::TyRef;
use bone_semantic::hir::{BindingKind, Function};
use bone_semantic::inference;
use bone_semantic::{hir::NodeRef, prelude::*};
use skull_parser::Ast;

use crate::types;

mod occurrences;
pub(crate) use occurrences::named_tokens;

pub(crate) fn source_ident(name: Ustr) -> &'static str {
    name.as_str().rsplit("::").next().unwrap_or(name.as_str())
}

// Keep feature-specific tie breakers: one shared winner would change
// navigation when a wrapper overlaps a name.
#[derive(Clone, Copy)]
enum NodePolicy {
    Narrowest,
    Hover,
}

struct ResolvedPosition<'a> {
    ws: &'a Workspace,
    module: &'a hir::Module,
    offset: usize,
    owner: Option<hir::BodyOwner>,
    body: Option<&'a Body>,
    bi: Option<&'a BodyInference>,
    node: Option<NodeRef>,
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
                    };
                    (outside_name, uninformative, iv.stop - iv.start)
                })
                .map(|iv| iv.val)
        });
        Some(Self {
            ws,
            module,
            offset,
            owner,
            body,
            bi,
            node,
        })
    }
}

pub fn definition_at<'t>(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
    text_of: TextOf<'_, 't>,
) -> Option<Span> {
    let at = ResolvedPosition::new(ws, mid, Some(inf), offset, NodePolicy::Narrowest)?;
    // A builtin type has no declaration; its node below names a landmark.
    if let Some(span) = at
        .symbol(text_of)
        .and_then(|symbol| symbol.definition(ws, text_of))
    {
        return Some(span);
    }
    let owner = at.owner?;
    match at.node? {
        // In a `FieldRef<Struct, V>` slot a `.field` ref names the struct's field.
        NodeRef::Expr(e) => match at.body?.expr_arena[e] {
            hir::Expr::SiblingRef(name) => inf
                .per_fn
                .get(&owner)
                .and_then(|bi| field_ref_struct(ws, bi, *bi.expr_types.get(e)?))
                .and_then(|sname| RefTarget::Field(RecordType::Struct(sname), name).declaration(ws))
                .or_else(|| def_of_name(ws, name)),
            _ => None,
        },
        NodeRef::StaticTy { mid, idx } => {
            type_def_target(ws, &ws.modules.get(mid.index())?.ty_arena[idx])
        }
        NodeRef::LocalTy(idx) => type_def_target(ws, &inf.per_fn.get(&owner)?.ty_arena[idx]),
        _ => None,
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
    let text_of = |file: &str| sources?.get(file).map(String::as_str);
    // A field access or a type annotation shows its type, with the
    // instantiated arguments the declaration lacks.
    let typed = match at.node {
        Some(NodeRef::Expr(e)) => at.body.is_some_and(|body| {
            matches!(
                body.expr_arena[e],
                hir::Expr::FieldAccess { .. } | hir::Expr::SafeNav { .. }
            )
        }),
        Some(NodeRef::LocalTy(_) | NodeRef::StaticTy { .. }) => true,
        _ => false,
    };
    if let Some(hover) = (!typed)
        .then(|| at.token(&text_of))
        .flatten()
        .and_then(|token| token.hover(ws, sources))
    {
        return Some(hover);
    }
    let module = at.module;
    if let (Some(body), Some(bi), Some(node)) = (at.body, at.bi, at.node) {
        match node {
            NodeRef::Expr(e) => {
                let span = *body.expr_spans.get(e)?;
                match &body.expr_arena[e] {
                    hir::Expr::Call {
                        callee: hir::Callee::Unresolved { name, name_span },
                        ..
                    } => {
                        let frag = crate::modules_scope::fragment(ws, module.file)?;
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
            NodeRef::Statement(..) => {}
        }
    }
    // A value-less global has no inference body, so resolve its type here.
    module.globals.values().find_map(|g| {
        let TyId::Static(tmid, idx) = g.ty else {
            return None;
        };
        let sp = *ws.modules.get(tmid.index())?.ty_spans.get(idx)?;
        span_contains(&sp, offset).then(|| (render_type_hover(ws, None, g.ty), sp))
    })
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
    let (edef, v) = variant_def(ws, enum_name, variant)?;
    Some(fenced(&format!(
        "{}::{}",
        display_name(edef.name.as_str()),
        render_variant_payload(ws, v)
    )))
}

/// Bare record heads keep the struct-literal HIR shape; a real struct is
/// never treated as a variant.
fn variant_def_span(ws: &Workspace, enum_name: Option<Ustr>, variant: Ustr) -> Option<Span> {
    variant_def(ws, enum_name, variant).map(|(_, v)| v.name_span)
}

fn variant_def(
    ws: &Workspace,
    enum_name: Option<Ustr>,
    variant: Ustr,
) -> Option<(&hir::EnumDef, &hir::EnumVariantDef)> {
    let edef = enum_def_for_variant(ws, enum_name, variant)?;
    Some((edef, edef.variants.iter().find(|v| v.name == variant)?))
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
        if struct_def_by_name(ws, name).is_some() {
            return format!("struct {}", display_name(name.as_str()));
        }
        if enum_def_by_name(ws, name).is_some() {
            return format!("enum {}", display_name(name.as_str()));
        }
    }
    ws.render(bi, ty)
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

        if let Some(s) = m.nominals.struct_id(name) {
            return Some(m.structs[s.def].name_span);
        }

        if let Some(en) = m.nominals.enum_id(name) {
            return Some(m.enums[en.def].name_span);
        }

        if let Some(nt) = m.nominals.newtype_id(name) {
            return Some(m.newtypes[nt.def].name_span);
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

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum RefTarget {
    Binding(ModuleId, hir::BodyOwner, Idx<hir::BindingInfo>),
    GenericParam(ModuleId, hir::VarId),
    EnumVariant(Ustr, Ustr),
    Enum(Ustr),
    Newtype(Ustr),
    Struct(Ustr),
    Field(RecordType, Ustr),
    Function(ModuleId, Idx<hir::Function>),
    Global(Ustr),
    Decorator(Ustr),
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
    ResolvedPosition::new(ws, mid, Some(inf), offset, NodePolicy::Narrowest)
        .and_then(|at| at.symbol(text_of))
        .map_or_else(Vec::new, |symbol| symbol.sites(ws, text_of, fn_scope))
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

fn decorator_declared(ws: &Workspace, name: Ustr) -> bool {
    ws.modules
        .iter()
        .any(|m| m.decorators.values().any(|d| d.def_key == name))
}

pub(crate) fn is_test_path(path: &str) -> bool {
    path.split(['/', '\\']).any(|seg| seg == "tests")
}

fn leading_name_span(span: &Span, name: Ustr) -> Span {
    let start = span.offset_range().0;
    span.with_offset_range((start, start + source_ident(name).len()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordType {
    Struct(Ustr),
    Variant(Ustr, Ustr),
}

#[derive(Clone, Copy)]
pub(crate) struct Token {
    target: RefTarget,
    span: Span,
}

impl Token {
    /// A declaration name shows its declaration; a use of a function or a
    /// global shows its value's type instead, from the node.
    pub(crate) fn hover(
        self,
        ws: &Workspace,
        sources: Option<&SourceMap>,
    ) -> Option<(String, Span)> {
        let text = match self.target {
            RefTarget::Function(fmid, fid) => {
                let f = &ws.module(fmid).functions[fid];
                (f.name_span == self.span)
                    .then(|| fenced(&render_fn_signature_with_defaults(ws, fmid, fid, sources)))
            }
            RefTarget::Global(name) => ws.modules.iter().find_map(|m| {
                let g = &m.globals[*m.global_by_name.get(&name)?];
                (g.name_span == self.span)
                    .then(|| format!("Type: {}", render_hover_type(ws, None, g.ty)))
            }),
            RefTarget::Newtype(name) => newtype_def_by_name(ws, name).map(|nt| {
                fenced(&format!(
                    "newtype {}({})",
                    display_name(nt.name.as_str()),
                    render_hover_type(ws, None, nt.wrapped)
                ))
            }),
            target => target.hover(ws),
        }?;
        Some((text, self.span))
    }
}

impl RefTarget {
    fn nominal(ws: &Workspace, name: Ustr) -> Self {
        if enum_def_by_name(ws, name).is_some() {
            Self::Enum(name)
        } else if newtype_def_by_name(ws, name).is_some() {
            Self::Newtype(name)
        } else {
            Self::Struct(name)
        }
    }

    fn definition<'t>(self, ws: &Workspace, text_of: TextOf<'_, 't>) -> Option<Span> {
        match self {
            Self::Binding(mid, owner, b) => {
                ws.module(mid).bodies[&owner].binding_spans.get(b).copied()
            }
            Self::GenericParam(mid, var) => {
                let m = ws.module(mid);
                declared_generics(m).find_map(|g| match *g {
                    TyId::Static(gm, gi)
                        if gm == mid
                            && matches!(m.ty_arena[gi], hir::TypeExpr::Generic(v, _) if v == var) =>
                    {
                        m.ty_spans.get(gi).copied()
                    }
                    _ => None,
                })
            }
            Self::Newtype(name) => newtype_def_by_name(ws, name).map(|def| def.name_span),
            Self::Function(mid, fid) => Some(ws.module(mid).functions[fid].name_span),
            Self::Global(name) => def_of_name(ws, name),
            Self::Decorator(name) => ws.modules.iter().find_map(|m| {
                m.decorators
                    .values()
                    .find(|d| d.def_key == name)
                    .and_then(|d| decorator_name_span(d, text_of))
            }),
            _ => self.declaration(ws),
        }
    }

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
        let record = if ws.nominals.enum_id(owner).is_some() {
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
            Self::Struct(name) => {
                Some(&module.structs[module.nominals.struct_id(name)?.def].fields)
            }
            Self::Variant(name, variant) => {
                let def = &module.enums[module.nominals.enum_id(name)?.def];
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

pub(crate) struct LocalBinding {
    pub name: Ustr,
    pub kind: hir::BindingKind,
    pub ty: Option<TyId>,
    declared_ty: Option<TyId>,
    pub decl_end: usize,
}

#[derive(Default)]
pub(crate) struct LocalScope<'w> {
    pub inference: Option<&'w inference::BodyInference>,
    pub bindings: Vec<LocalBinding>,
}

impl<'w> LocalScope<'w> {
    pub(crate) fn at(
        ws: &Workspace,
        mid: ModuleId,
        inf: Option<&'w InferenceResult>,
        offset: usize,
    ) -> Self {
        let module = ws.module(mid);
        let Some(owner) = module.body_at(offset) else {
            return Self::default();
        };
        let Some(body) = module.bodies.get(&owner) else {
            return Self::default();
        };
        let Some(facts) = &body.lexical_facts else {
            return Self::default();
        };
        let Some(&(_, scope, _)) = facts
            .regions
            .iter()
            .filter(|(span, _, include_end)| {
                let (start, end) = span.offset_range();
                span.file == module.file
                    && start <= offset
                    && (offset < end || *include_end && offset == end)
            })
            .min_by_key(|(span, scope, _)| {
                let (start, end) = span.offset_range();
                (end - start, std::cmp::Reverse(scope.into_raw().into_u32()))
            })
        else {
            return Self::default();
        };

        let mut ancestors = vec![scope];
        while let Some(parent) = body.scope_arena[*ancestors.last().unwrap()].parent {
            ancestors.push(parent);
        }
        let mut active = FxHashMap::default();
        for (idx, &(scope, activation)) in facts.bindings.iter() {
            if activation <= offset
                && let Some(depth) = ancestors.iter().position(|&s| s == scope)
            {
                let priority = (std::cmp::Reverse(depth), idx);
                let name = body.binding_arena[idx].name;
                let best = active.entry(name).or_insert(priority);
                *best = (*best).max(priority);
            }
        }
        let bi = inf.and_then(|inf| inf.per_fn.get(&owner));
        let mut bindings: Vec<_> = active.into_values().map(|(_, idx)| idx).collect();
        bindings.sort_unstable();
        Self {
            inference: bi,
            bindings: bindings
                .into_iter()
                .map(|idx| {
                    let binding = &body.binding_arena[idx];
                    LocalBinding {
                        name: binding.name,
                        kind: binding.kind,
                        ty: bi.and_then(|bi| bi.binding_types.get(idx).copied()),
                        declared_ty: binding.declared_ty,
                        decl_end: body.binding_spans[idx].offset_range().1,
                    }
                })
                .collect(),
        }
    }

    pub(crate) fn get(&self, name: Ustr) -> Option<&LocalBinding> {
        self.bindings.iter().find(|binding| binding.name == name)
    }

    pub(crate) fn render(&self, ws: &Workspace, binding: &LocalBinding) -> String {
        binding
            .ty
            .map_or_else(String::new, |ty| ws.render(self.inference, ty))
    }
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
    let locals = LocalScope::at(ws, mid, ws.inference(mid), offset);
    let head = Ustr::from(head);
    if let Some(binding) = locals.get(head) {
        let ty = binding.ty.or(binding.declared_ty)?;
        return Some(TyRef::new(locals.inference, ty));
    }
    let frag = crate::modules_scope::fragment(ws, module.file)?;
    let key = bone_semantic::lower::resolve::resolve_bare(ws, frag, head)
        .ok()?
        .key;
    Some(TyRef::of_static(*ws.globals_by_name.get(&key)?))
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
    LocalScope::at(ws, mid, Some(inf), offset)
        .get(Ustr::from(name))?
        .ty
}

/// One-segment record paths stay with HIR, which can distinguish a struct
/// from a variant.
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
    let id = ws.nominals.struct_id(name)?;
    Some(&ws.module(id.module).structs[id.def])
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
        .find_map(|m| m.nominals.newtype_id(name).map(|i| &m.newtypes[i.def]))
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
    let record = RecordType::Variant(Ustr::from(enum_name), Ustr::from(variant));
    ws.modules
        .iter()
        .find_map(|m| record.fields(m))
        .map_or_else(Vec::new, |fields| {
            fields
                .iter()
                .map(|f| (f.name.to_string(), ws.render(None, f.ty)))
                .collect()
        })
}

fn smallest_expr_covering(
    body: &Body,
    offset: usize,
    matches: impl Fn(Idx<hir::Expr>, &hir::Expr) -> bool,
) -> Option<Idx<hir::Expr>> {
    body.expr_arena
        .iter()
        .filter(|&(e, expr)| matches(e, expr))
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

    let e = smallest_expr_covering(body, offset, |_, expr| {
        matches!(expr, hir::Expr::Match { .. })
    })?;
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

    let e = smallest_expr_covering(body, offset, |_, expr| {
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
    let scope = LocalScope::at(ws, mid, Some(inf), offset);
    let innermost_ty = |name| scope.get(name).and_then(|binding| binding.ty);

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
                        } => ws.nominals.newtype_id(name).is_some(),
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

    let e = smallest_expr_covering(body, offset, |e, expr| {
        matches!(
            expr,
            hir::Expr::Call {
                callee: hir::Callee::Unresolved { .. },
                ..
            }
        ) && !bi.resolved_overload_calls.contains_key(&e)
    })?;
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
    let id = ws.nominals.enum_id(name)?;
    Some(&ws.module(id.module).enums[id.def])
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
    Type(SymCat),
    Enum(Vec<VisibleEnumVariant>),
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
    } else if let Some(sid) = ws.nominals.struct_id(key) {
        (
            VisibleBindingKind::Type(SymCat::Struct),
            ws.module(sid.module).file,
        )
    } else if let Some(eid) = ws.nominals.enum_id(key) {
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
    } else if let Some(nid) = ws.nominals.newtype_id(key) {
        (
            VisibleBindingKind::Type(SymCat::Newtype),
            ws.module(nid.module).file,
        )
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

impl VisibleBinding {
    fn emit(
        &self,
        ws: &Workspace,
        frag: &bone_core::modules::FragmentScope,
        expr: bool,
        pfx: &[u8],
        show_private: bool,
        test_path_cache: &mut FxHashMap<Ustr, bool>,
        mut push: impl FnMut(CompletionEntry),
    ) {
        let visible = |name: Ustr| show_private || !name.as_str().starts_with('_');
        let matches = |name: Ustr| crate::completion::match_policy::admit(name.as_bytes(), pfx);
        let mut def_file = self.def_file;
        let mut file_is_test = self.file_is_test;
        if let VisibleBindingKind::Enum(variants) = &self.kind
            && expr
            && !def_file.as_str().starts_with('<')
        {
            for variant in variants {
                if visible(variant.suffix)
                    && (matches(variant.suffix) || matches(variant.qualified))
                {
                    push(CompletionEntry {
                        detail: Some(self.name.to_string()),
                        test_scoped: file_is_test
                            || variant.qualified.as_str().starts_with("test_"),
                        ..CompletionEntry::new(
                            variant.qualified,
                            CompletionSymbol::Variant {
                                enum_key: self.key,
                                shape: variant.shape,
                            },
                            def_file,
                        )
                    });
                }
            }
            return;
        }
        if !visible(self.name) || !matches(self.name) {
            return;
        }
        let symbol = match self.kind {
            VisibleBindingKind::Type(kind) => CompletionSymbol::Type(kind),
            VisibleBindingKind::Enum(_) => CompletionSymbol::Type(SymCat::Enum),
            VisibleBindingKind::Function {
                mut takes_params,
                multiple_overloads,
            } if expr => {
                if multiple_overloads {
                    let Some((mid, fid)) = first_callable_in_scope(ws, frag, self.key) else {
                        return;
                    };
                    def_file = ws.module(mid).file;
                    file_is_test = *test_path_cache
                        .entry(def_file)
                        .or_insert_with(|| is_test_path(def_file.as_str()));
                    takes_params = !ws.module(mid).functions[fid].param_decls.is_empty();
                }
                CompletionSymbol::Function {
                    key: self.key,
                    takes_params,
                }
            }
            // Arena IDs only enter the request, never the cached scope.
            VisibleBindingKind::Global if expr => {
                let Some(&ty) = ws.globals_by_name.get(&self.key) else {
                    return;
                };
                CompletionSymbol::Variable {
                    kind: SymCat::Global,
                    ty: Some(ty),
                    rec: 0,
                }
            }
            _ => return,
        };
        push(CompletionEntry {
            test_scoped: file_is_test || self.name.as_str().starts_with("test_"),
            ..CompletionEntry::new(self.name, symbol, def_file)
        });
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
        binding.emit(
            ws,
            frag,
            expr_position,
            pfx,
            show_private,
            test_path_cache,
            |entry| out.push(entry),
        );
    }
}

pub(crate) fn completion_with_visible_bindings(
    ws: &Workspace,
    req_file: Ustr,
    frag: &bone_core::modules::FragmentScope,
    locals: &LocalScope<'_>,
    prefix: &str,
    expr_position: bool,
    is_coroutine: bool,
    visible_bindings: &[VisibleBinding],
) -> Vec<CompletionEntry> {
    let mut seen = FxHashSet::default();
    let mut out = Vec::new();
    let pfx = prefix.as_bytes();
    let show_private = prefix.starts_with('_');
    let matches = |name: &str| {
        (show_private || !name.starts_with('_'))
            && crate::completion::match_policy::admit(name.as_bytes(), pfx)
    };

    if expr_position {
        // Params carry the earliest spans, so an inner `let` beats an enclosing
        // param on ties.
        let mut by_decl: Vec<usize> = (0..locals.bindings.len()).collect();
        by_decl.sort_by_key(|&i| std::cmp::Reverse(locals.bindings[i].decl_end));
        let mut rec_of = vec![0u8; locals.bindings.len()];
        for (rank, &i) in by_decl.iter().enumerate() {
            rec_of[i] = rank.min(9) as u8;
        }
        for (i, local) in locals.bindings.iter().enumerate() {
            let name = local.name;
            if !matches(name.as_str()) {
                continue;
            }
            if seen.insert(name) {
                let kind = if local.kind == BindingKind::Param {
                    SymCat::Param
                } else {
                    SymCat::Local
                };
                out.push(CompletionEntry {
                    detail: local
                        .ty
                        .map(|ty| ws.render(locals.inference, ty))
                        .filter(|ty| !ty.is_empty()),
                    ..CompletionEntry::new(
                        name,
                        CompletionSymbol::Variable {
                            kind,
                            ty: local.ty,
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
        binding.emit(
            ws,
            frag,
            expr_position,
            pfx,
            show_private,
            &mut test_path_cache,
            |entry| {
                if seen.insert(entry.name) {
                    out.push(entry);
                }
            },
        );
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

    if expr_position && is_coroutine {
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
    let scope = LocalScope::at(ws, mid, Some(inf), offset);
    let in_scope = scope
        .bindings
        .iter()
        .map(|binding| (binding.name.to_string(), scope.render(ws, binding)))
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
