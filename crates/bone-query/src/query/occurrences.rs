//! Every name token in a module and the symbol it names. Cursor resolution
//! (`symbol`) and find-references (`sites`) both read this one walk.

use super::*;

/// One name token. `hit` is where the cursor resolves to `target`; `site` is
/// the token references report and rename rewrites. Either may be absent.
#[derive(Clone, Copy)]
struct Occurrence {
    target: RefTarget,
    /// Lower wins when several hits cover the cursor; ties keep the first.
    rank: i8,
    hit: Hit,
    site: Option<RefSite>,
    /// A function named as a value: no argument list picks an overload, so it
    /// matches every overload by name.
    by_name: bool,
}

/// Where the cursor resolves to an occurrence. A node hit needs that node to be
/// the narrowest at the cursor; its index interval can be wider than the
/// token (`return s` for `s`).
#[derive(Clone, Copy)]
enum Hit {
    Nowhere,
    Span(Span),
    Node(NodeRef),
    SpanOnNode(Span, NodeRef),
}

/// Which bodies to walk; module-level declarations are always walked.
#[derive(Clone, Copy)]
enum Bodies {
    All,
    Only(Option<hir::BodyOwner>),
}

impl Occurrence {
    fn new(target: RefTarget, rank: i8) -> Self {
        Occurrence {
            target,
            rank,
            hit: Hit::Nowhere,
            site: None,
            by_name: false,
        }
    }

    fn token(span: Span, target: RefTarget, rank: i8) -> Self {
        Self::new(target, rank).hit(span).site(span)
    }

    fn hit(mut self, span: impl Into<Option<Span>>) -> Self {
        self.hit = span.into().map_or(Hit::Nowhere, Hit::Span);
        self
    }

    fn on(mut self, node: impl Into<Option<NodeRef>>) -> Self {
        self.hit = node.into().map_or(Hit::Nowhere, Hit::Node);
        self
    }

    /// The span hit counts only on its node.
    fn within(mut self, node: NodeRef) -> Self {
        if let Hit::Span(span) = self.hit {
            self.hit = Hit::SpanOnNode(span, node);
        }
        self
    }

    fn site(mut self, span: impl Into<Option<Span>>) -> Self {
        self.site = span.into().map(RefSite::plain);
        self
    }

    fn shorthand(mut self, span: Span, shorthand: Option<Shorthand>) -> Self {
        self.site = Some(RefSite { span, shorthand });
        self
    }
}

fn tail_span(span: &Span, len: usize) -> Span {
    let end = span.offset_range().1;
    span.with_offset_range((end - len, end))
}

/// The field name ends an access but not its span, which may close a
/// parenthesis, and a Lua keyword field is stored escaped.
fn field_token(text: &str, span: &Span, field: Ustr) -> Option<Span> {
    let name = rock_tokenizer::Tokenizer::unescape_lua_keyword(field.as_str());
    let (from, to) = span.offset_range();
    let access = text
        .get(from..to)?
        .trim_end_matches(|c: char| c == ')' || c.is_whitespace());
    let head = access.strip_suffix(name)?;
    head.trim_end().ends_with('.').then(|| {
        let start = from + head.len();
        span.with_offset_range((start, start + name.len()))
    })
}

/// Narrows a walk to one symbol: tokens spelled otherwise are skipped before
/// they are resolved, and type annotations are visited only for a type.
#[derive(Clone, Copy)]
struct Filter {
    name: Ustr,
    types: bool,
}

fn walk_module<'t>(
    ws: &Workspace,
    module: &hir::Module,
    bodies: Bodies,
    filter: Option<Filter>,
    text_of: TextOf<'_, 't>,
    emit: &mut dyn FnMut(Occurrence),
) {
    let want = |n: Ustr| filter.is_none_or(|f| f.name == n);
    let types = filter.is_none_or(|f| f.types);
    let mid = module.id;
    let mut declared = Vec::new();
    for ty in declared_generics(module) {
        let TyId::Static(gmid, idx) = *ty else {
            continue;
        };
        let Some(gm) = ws.modules.get(gmid.index()) else {
            continue;
        };
        let hir::TypeExpr::Generic(var, name) = gm.ty_arena[idx] else {
            continue;
        };
        if !want(name) {
            continue;
        }
        let span = gm.ty_spans.get(idx).copied();
        let first = gmid == mid && !declared.contains(&var);
        if first {
            declared.push(var);
        }
        let site = span.filter(|_| first).map(|s| leading_name_span(&s, name));
        emit(
            Occurrence::new(RefTarget::GenericParam(gmid, var), -1)
                .hit(span)
                .site(site),
        );
    }
    for en in module.enums.values() {
        if want(en.name) {
            let name_span = leading_name_span(&en.name_span, en.name);
            emit(Occurrence::token(name_span, RefTarget::Enum(en.name), 10));
        }
        for variant in &en.variants {
            if want(variant.name) {
                let target = RefTarget::EnumVariant(en.name, variant.name);
                emit(Occurrence::token(variant.name_span, target, 0));
            }
            if let hir::VariantPayload::Record(fields) = &variant.payload {
                let record = RecordType::Variant(en.name, variant.name);
                for field in fields.iter().filter(|f| want(f.name)) {
                    let target = RefTarget::Field(record, field.name);
                    emit(Occurrence::token(field.span, target, 50));
                }
            }
        }
    }
    for nt in module.newtypes.values().filter(|nt| want(nt.name)) {
        emit(Occurrence::token(
            nt.name_span,
            RefTarget::Newtype(nt.name),
            20,
        ));
    }
    for record in module.structs.values() {
        if want(record.name) {
            let name_span = leading_name_span(&record.name_span, record.name);
            emit(Occurrence::token(
                name_span,
                RefTarget::Struct(record.name),
                30,
            ));
        }
        for field in record.fields.iter().filter(|f| want(f.name)) {
            let target = RefTarget::Field(RecordType::Struct(record.name), field.name);
            emit(Occurrence::token(field.span, target, 40));
        }
    }
    for (fid, function) in module.functions.iter() {
        if want(function.name) {
            let named = !source_ident(function.name).starts_with("__");
            emit(
                Occurrence::new(RefTarget::Function(mid, fid), 61)
                    .hit(named.then_some(function.name_span))
                    .site(function.name_span),
            );
        }
        for annotation in &function.annotations {
            let key = annotation.resolved_name.unwrap_or(annotation.name.name);
            if !want(key) {
                continue;
            }
            let span = annotation.name.span;
            emit(
                Occurrence::new(RefTarget::Decorator(key), 81)
                    .hit(decorator_declared(ws, key).then_some(span))
                    .site(span),
            );
        }
    }
    for global in module.globals.values().filter(|g| want(g.name)) {
        let target = RefTarget::Global(global.name);
        emit(Occurrence::token(global.name_span, target, 70));
    }
    for decorator in module.decorators.values().filter(|d| want(d.def_key)) {
        if let Some(span) = decorator_name_span(decorator, text_of) {
            emit(Occurrence::token(
                span,
                RefTarget::Decorator(decorator.def_key),
                80,
            ));
        }
    }
    let inf = ws.inference(mid);
    for (owner, body) in &module.bodies {
        if matches!(bodies, Bodies::Only(only) if only != Some(*owner)) {
            continue;
        }
        let bi = inf.and_then(|inf| inf.per_fn.get(owner));
        walk_body(ws, module, *owner, body, bi, &want, types, text_of, emit);
    }
}

fn walk_body<'t>(
    ws: &Workspace,
    module: &hir::Module,
    owner: hir::BodyOwner,
    body: &Body,
    bi: Option<&BodyInference>,
    want: &dyn Fn(Ustr) -> bool,
    types: bool,
    text_of: TextOf<'_, 't>,
    emit: &mut dyn FnMut(Occurrence),
) {
    let mid = module.id;
    // `ty_spans` skips bare `Ident`-kind types, so type names come from
    // `node_index`, which keeps a `p: Point` annotation.
    for iv in body.node_index().iter().filter(|_| types) {
        let span = Span::new(module.file, (0, 0), (0, 0), (iv.start, iv.stop));
        let (arena, ty_span, idx) = match iv.val {
            NodeRef::StaticTy { mid: ty_mid, idx } => {
                let Some(m) = ws.modules.get(ty_mid.index()) else {
                    continue;
                };
                let generic = match m.ty_arena[idx] {
                    hir::TypeExpr::Nullable(inner) => inner,
                    _ => idx,
                };
                if let hir::TypeExpr::Generic(var, name) = m.ty_arena[generic]
                    && want(name)
                {
                    let site = (ty_mid == mid).then(|| leading_name_span(&span, name));
                    emit(
                        Occurrence::new(RefTarget::GenericParam(ty_mid, var), -1)
                            .on(iv.val)
                            .site(site),
                    );
                }
                (&m.ty_arena, m.ty_spans.get(idx), idx)
            }
            NodeRef::LocalTy(idx) => (&body.lowered_tys, body.lowered_ty_spans.get(idx), idx),
            _ => continue,
        };
        let Some(name) = types::annotation_nominal(arena, idx).filter(|n| want(*n)) else {
            continue;
        };
        let target = RefTarget::nominal(ws, name);
        let hit = match arena[idx] {
            hir::TypeExpr::App { .. } => ty_span.map(|s| leading_name_span(s, name)),
            _ => None,
        };
        let site = leading_name_span(&span, name);
        // A newtype annotation may spell an alias rather than the newtype.
        let spelled = !matches!(target, RefTarget::Newtype(_))
            || text_of(module.file.as_str())
                .is_none_or(|text| span_text(text, &site) == Some(source_ident(name)));
        emit(
            Occurrence::new(target, 31)
                .hit(hit)
                .within(iv.val)
                .site(spelled.then_some(site)),
        );
    }

    let binding = |b| RefTarget::Binding(mid, owner, b);
    let live = |b| body.deco_target != Some(b);
    // Shorthand iff a same-named field's name token is this site's token;
    // explicit `x: x` has the field name on a different token.
    let field_tokens: FxHashSet<(Ustr, (usize, usize))> = body
        .struct_lit_field_arena
        .values()
        .map(|f| (f.name, f.name_span.offset_range()))
        .chain(
            body.record_pat_field_arena
                .values()
                .map(|f| (f.name, f.name_span.offset_range())),
        )
        .filter(|(name, _)| want(*name))
        .collect();
    let shorthand_at = |span: &Span, name: Ustr| {
        field_tokens
            .contains(&(name, span.offset_range()))
            .then_some(Shorthand::KeepField(name))
    };
    for (b, info) in body.binding_arena.iter() {
        let Some(span) = body.binding_spans.get(b).filter(|_| want(info.name)) else {
            continue;
        };
        // A param's binding span covers `name: type`; the site is the name.
        let site = leading_name_span(span, info.name);
        emit(
            Occurrence::new(binding(b), -2)
                .on(live(b).then_some(NodeRef::Binding(b)))
                .shorthand(site, shorthand_at(&site, info.name)),
        );
    }

    for (e, node) in body.expr_arena.iter() {
        let span = body.expr_spans.get(e).copied();
        let this = NodeRef::Expr(e);
        match *node {
            hir::Expr::Var(b)
            | hir::Expr::Call {
                callee: hir::Callee::Local(b),
                ..
            } if !want(body.binding_arena[b].name) => {}
            hir::Expr::Var(b) => {
                let mut occ = Occurrence::new(binding(b), -2).on(live(b).then_some(this));
                if let Some(span) = span {
                    occ = occ.shorthand(span, shorthand_at(&span, body.binding_arena[b].name));
                }
                emit(occ);
            }
            hir::Expr::Call {
                callee: hir::Callee::Local(b),
                ..
            } => {
                let site = span.map(|span| leading_name_span(&span, body.binding_arena[b].name));
                emit(
                    Occurrence::new(binding(b), -2)
                        .on(live(b).then_some(this))
                        .site(site),
                );
            }
            hir::Expr::Call {
                callee: hir::Callee::Unresolved { name, name_span },
                ..
            } if want(name) => {
                let site = body.name_spans[name_span];
                emit(Occurrence::new(RefTarget::Newtype(name), 31).site(site));
            }
            hir::Expr::StructLiteral { name, .. } if want(name) => {
                if let Some(span) = span {
                    let head = leading_name_span(&span, name);
                    emit(Occurrence::token(head, RefTarget::nominal(ws, name), 31).within(this));
                }
            }
            hir::Expr::EnumVariant {
                enum_name,
                enum_name_span,
                variant,
                variant_span,
                ..
            } => {
                let variant_span = body.name_spans[variant_span];
                if want(variant) {
                    let target = RefTarget::EnumVariant(enum_name, variant);
                    emit(Occurrence::token(variant_span, target, 1));
                }
                if want(enum_name) {
                    let enum_span = body.name_spans[enum_name_span];
                    let qualifier = (enum_span.offset_range().0, variant_span.offset_range().0);
                    emit(
                        Occurrence::new(RefTarget::Enum(enum_name), 11)
                            .hit(enum_span.with_offset_range(qualifier))
                            .site(leading_name_span(&enum_span, enum_name)),
                    );
                }
            }
            hir::Expr::FieldAccess { base, field } | hir::Expr::SafeNav { base, field }
                if want(field) =>
            {
                let record =
                    bi.and_then(|bi| types::nominal_name(ws, Some(bi), *bi.expr_types.get(base)?));
                let token =
                    span.and_then(|span| field_token(text_of(span.file.as_str())?, &span, field));
                if let (Some(record), Some(token)) = (record, token) {
                    let hit = matches!(node, hir::Expr::FieldAccess { .. }).then_some(token);
                    emit(
                        Occurrence::new(RefTarget::Field(RecordType::Struct(record), field), 41)
                            .hit(hit)
                            .within(this)
                            .site(token),
                    );
                }
            }
            hir::Expr::SiblingRef(field) if want(field) => {
                let record = match owner {
                    hir::BodyOwner::StructDefaults(sidx) => Some(module.structs[sidx].name),
                    _ => bi.and_then(|bi| field_ref_struct(ws, bi, *bi.expr_types.get(e)?)),
                };
                if let (Some(record), Some(span)) = (record, span) {
                    let target = RefTarget::Field(RecordType::Struct(record), field);
                    emit(Occurrence::new(target, 41).site(tail_span(&span, field.len())));
                }
            }
            hir::Expr::Global(name) if want(name) => {
                global_occurrences(ws, name, span, this, bi.is_some(), emit);
            }
            _ => {}
        }
        if let Some((record, fields)) = RecordType::literal(node) {
            // Struct-literal fields resolve only on the literal itself.
            let (rank, gate) = match record {
                RecordType::Struct(_) => (41, Some(this)),
                RecordType::Variant(..) => (51, None),
            };
            for idx in fields {
                let f = &body.struct_lit_field_arena[idx];
                if !want(f.name) {
                    continue;
                }
                let shorthand = body
                    .expr_spans
                    .get(f.value)
                    .is_some_and(|span| span.offset_range() == f.name_span.offset_range());
                // A struct field's label resolves up to its value, colon included.
                let value_start = body.expr_spans.get(f.value).map(|v| v.offset_range().0);
                let hit = match (record, value_start) {
                    (RecordType::Struct(_), Some(start))
                        if start > f.name_span.offset_range().1 =>
                    {
                        f.name_span
                            .with_offset_range((f.name_span.offset_range().0, start))
                    }
                    _ => f.name_span,
                };
                let mut occ = Occurrence::new(RefTarget::Field(record, f.name), rank)
                    .hit(hit)
                    .shorthand(
                        f.name_span,
                        shorthand.then_some(Shorthand::KeepBinding(f.name)),
                    );
                if let Some(gate) = gate {
                    occ = occ.within(gate);
                }
                emit(occ);
            }
        }
    }
    for (&e, &(cmid, cfid)) in bi.iter().flat_map(|bi| &bi.resolved_overload_calls) {
        if want(ws.module(cmid).functions[cfid].name) {
            emit(
                Occurrence::new(RefTarget::Function(cmid, cfid), 59)
                    .on(NodeRef::Expr(e))
                    .site(call_name_span(ws, body, e, cmid, cfid, text_of)),
            );
        }
        named_argument_sites(ws, body, e, cmid, cfid, want, text_of, emit);
    }

    for (id, pattern) in body.pattern_arena.iter() {
        match *pattern {
            hir::Pattern::EnumVariant {
                enum_name,
                enum_name_span,
                variant,
                variant_span,
                ..
            } => {
                if let Some(en) =
                    enum_def_for_variant(ws, enum_name, variant).filter(|_| want(variant))
                {
                    let target = RefTarget::EnumVariant(en.name, variant);
                    emit(Occurrence::token(variant_span, target, 2));
                }
                if let (Some(name), Some(span)) = (enum_name.filter(|n| want(*n)), enum_name_span) {
                    emit(enum_qualifier(name, span, Some(variant_span)));
                }
            }
            hir::Pattern::Record {
                enum_name: Some(en),
                enum_name_span,
                name,
                name_span,
                ..
            } => {
                if let Some(span) = enum_name_span.filter(|_| want(en)) {
                    emit(enum_qualifier(en, span, name_span));
                }
                if let (Some(variant), Some(span)) = (name.filter(|n| want(*n)), name_span) {
                    emit(Occurrence::token(
                        span,
                        RefTarget::EnumVariant(en, variant),
                        2,
                    ));
                }
            }
            hir::Pattern::Record {
                enum_name: None,
                name_span: Some(span),
                ..
            } if types => {
                if let Some((RecordType::Struct(name), _)) = RecordType::pattern(ws, body, bi, id)
                    && want(name)
                {
                    emit(Occurrence::token(span, RefTarget::Struct(name), 32));
                }
            }
            _ => {}
        }
        let wanted_field = || match body.pattern_arena[id] {
            hir::Pattern::Record { fields, .. } => fields
                .into_iter()
                .any(|f| want(body.record_pat_field_arena[f].name)),
            _ => false,
        };
        if let Some((record, fields)) = wanted_field()
            .then(|| RecordType::pattern(ws, body, bi, id))
            .flatten()
        {
            let rank = match record {
                RecordType::Struct(_) => 42,
                RecordType::Variant(..) => 52,
            };
            for idx in fields {
                let f = &body.record_pat_field_arena[idx];
                if !want(f.name) {
                    continue;
                }
                let shorthand = pattern_field_is_shorthand(body, f);
                emit(
                    Occurrence::new(RefTarget::Field(record, f.name), rank)
                        .hit((!shorthand).then_some(f.name_span))
                        .shorthand(
                            f.name_span,
                            shorthand.then_some(Shorthand::KeepBinding(f.name)),
                        ),
                );
            }
        }
    }
}

/// Every name token of a module's declarations and of the bodies `keep`
/// accepts: the body it is in, its symbol, its span and the node it resolves on.
pub(crate) fn named_tokens<'t>(
    ws: &Workspace,
    module: &hir::Module,
    keep: &dyn Fn(hir::BodyOwner) -> bool,
    text_of: TextOf<'_, 't>,
    each: &mut dyn FnMut(Option<hir::BodyOwner>, RefTarget, Span, Option<NodeRef>),
) {
    let owner = std::cell::Cell::new(None);
    let mut emit = |occ: Occurrence| {
        let node = match occ.hit {
            Hit::Node(node) | Hit::SpanOnNode(_, node) => Some(node),
            Hit::Nowhere | Hit::Span(_) => None,
        };
        if let Some(site) = occ.site {
            each(owner.get(), occ.target, site.span, node);
        }
    };
    walk_module(ws, module, Bodies::Only(None), None, text_of, &mut emit);
    let inf = ws.inference(module.id);
    let want = |_| true;
    for (body_owner, body) in module.bodies.iter().filter(|(o, _)| keep(**o)) {
        owner.set(Some(*body_owner));
        let bi = inf.and_then(|inf| inf.per_fn.get(body_owner));
        walk_body(
            ws,
            module,
            *body_owner,
            body,
            bi,
            &want,
            true,
            text_of,
            &mut emit,
        );
    }
}

/// The qualifier resolves up to its variant, `::` included.
fn enum_qualifier(name: Ustr, span: Span, variant: Option<Span>) -> Occurrence {
    let (start, end) = span.offset_range();
    let end = variant.map_or(end, |v| v.offset_range().0.max(end));
    Occurrence::new(RefTarget::Enum(name), 12)
        .hit(span.with_offset_range((start, end)))
        .site(leading_name_span(&span, name))
}

/// A bare top-level name: a global, or a function used as a value.
fn global_occurrences(
    ws: &Workspace,
    name: Ustr,
    span: Option<Span>,
    node: NodeRef,
    inferred: bool,
    emit: &mut dyn FnMut(Occurrence),
) {
    let function = ws.functions_by_name.get(&name).and_then(|f| f.first());
    if let Some(&(fmid, fid)) = function {
        let mut occ = Occurrence::new(RefTarget::Function(fmid, fid), 60)
            .on((!name.as_str().starts_with("__")).then_some(node))
            .site(span.filter(|_| inferred));
        occ.by_name = true;
        emit(occ);
    }
    let global = ws.globals_by_name.contains_key(&name);
    emit(
        Occurrence::new(RefTarget::Global(name), 71)
            .on(global.then_some(node))
            .site(span.map(|span| tail_span(&span, source_ident(name).len()))),
    );
}

/// The callee token of a resolved call.
fn call_name_span<'t>(
    ws: &Workspace,
    body: &Body,
    call: Idx<hir::Expr>,
    cmid: ModuleId,
    cfid: Idx<hir::Function>,
    text_of: TextOf<'_, 't>,
) -> Option<Span> {
    match body.expr_arena[call] {
        hir::Expr::Call {
            callee: hir::Callee::Unresolved { name_span, .. },
            ..
        } => Some(body.name_spans[name_span]),
        // HIR keeps no method-token span for `recv?.method()`; find the name in
        // source after the receiver.
        hir::Expr::SafeNavCall { receiver, .. } => {
            let sp = body.expr_spans.get(call)?;
            let rsp = body.expr_spans.get(receiver)?;
            let name = ws.modules.get(cmid.index())?.functions[cfid].name;
            ident_span_in(
                text_of(sp.file.as_str())?,
                rsp.offset_range().1,
                sp.offset_range().1,
                source_ident(name),
                sp,
            )
        }
        _ => None,
    }
}

/// A named-argument label names the callee's parameter.
fn named_argument_sites<'t>(
    ws: &Workspace,
    body: &Body,
    call: Idx<hir::Expr>,
    cmid: ModuleId,
    cfid: Idx<hir::Function>,
    want: &dyn Fn(Ustr) -> bool,
    text_of: TextOf<'_, 't>,
    emit: &mut dyn FnMut(Occurrence),
) {
    let owner = hir::BodyOwner::Function(cfid);
    let Some(callee) = ws
        .modules
        .get(cmid.index())
        .and_then(|m| m.bodies.get(&owner))
    else {
        return;
    };
    for (label, value) in named_arguments(body, call).filter(|(label, _)| want(*label)) {
        let param = callee
            .param_bindings
            .into_iter()
            .find(|p| callee.binding_arena[*p].name == label);
        if let (Some(param), Some(span)) = (param, named_argument_span(text_of, label, value)) {
            emit(Occurrence::token(
                span,
                RefTarget::Binding(cmid, owner, param),
                -2,
            ));
        }
    }
}

impl ResolvedPosition<'_> {
    pub(super) fn symbol<'t>(&self, text_of: TextOf<'_, 't>) -> Option<RefTarget> {
        self.resolve(text_of).map(|occ| occ.target)
    }

    /// The symbol at the cursor and the token naming it there, when there is
    /// one.
    pub(super) fn token<'t>(&self, text_of: TextOf<'_, 't>) -> Option<Token> {
        let occ = self.resolve(text_of)?;
        let span = match (occ.site, occ.hit) {
            (Some(site), _) => site.span,
            (None, Hit::Span(span) | Hit::SpanOnNode(span, _)) => span,
            _ => return None,
        };
        Some(Token {
            target: occ.target,
            span,
        })
    }

    fn resolve<'t>(&self, text_of: TextOf<'_, 't>) -> Option<Occurrence> {
        let mut found: Option<Occurrence> = None;
        let bodies = Bodies::Only(self.owner);
        walk_module(self.ws, self.module, bodies, None, text_of, &mut |occ| {
            let on = match occ.hit {
                Hit::Nowhere => false,
                Hit::Span(span) => span_contains(&span, self.offset),
                Hit::Node(node) => self.node == Some(node),
                Hit::SpanOnNode(span, node) => {
                    self.node == Some(node) && span_contains(&span, self.offset)
                }
            };
            if on && found.is_none_or(|found| occ.rank < found.rank) {
                found = Some(occ);
            }
        });
        found
    }
}

impl RefTarget {
    /// The spelling every site shares (a function's key, not its source name).
    fn name(self, ws: &Workspace) -> Ustr {
        match self {
            Self::Binding(mid, owner, b) => ws.module(mid).bodies[&owner].binding_arena[b].name,
            Self::GenericParam(mid, var) => ws
                .module(mid)
                .ty_arena
                .values()
                .find_map(|te| match *te {
                    hir::TypeExpr::Generic(v, name) if v == var => Some(name),
                    _ => None,
                })
                .unwrap_or_default(),
            Self::EnumVariant(_, name)
            | Self::Enum(name)
            | Self::Newtype(name)
            | Self::Struct(name)
            | Self::Field(_, name)
            | Self::Global(name)
            | Self::Decorator(name) => name,
            Self::Function(mid, fid) => ws.module(mid).functions[fid].name,
        }
    }

    /// Every site of the symbol by file and position; the first sighting of a
    /// token wins.
    pub(super) fn sites<'t>(
        self,
        ws: &Workspace,
        text_of: TextOf<'_, 't>,
        scope: FnRefScope,
    ) -> Vec<RefSite> {
        // Renaming a user function leaves read-only (stdlib) overloads alone.
        let function = match self {
            Self::Function(fmid, fid) => {
                let module = ws.module(fmid);
                let owned_only =
                    scope == FnRefScope::AllOverloads && !is_read_only_source(module.file.as_str());
                Some((module.functions[fid].name, owned_only))
            }
            _ => None,
        };
        let filter = Some(Filter {
            name: self.name(ws),
            types: matches!(
                self,
                Self::GenericParam(..) | Self::Enum(_) | Self::Newtype(_) | Self::Struct(_)
            ),
        });
        let mut sites = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // A local is named only in its body; a parameter also labels named
        // arguments, and a generic is named only in its own module.
        let (home, bodies) = match self {
            Self::Binding(mid, owner, b)
                if !ws.module(mid).bodies[&owner]
                    .param_bindings
                    .into_iter()
                    .any(|p| p == b) =>
            {
                (Some(mid), Bodies::Only(Some(owner)))
            }
            Self::GenericParam(mid, _) => (Some(mid), Bodies::All),
            _ => (None, Bodies::All),
        };
        let modules = ws
            .modules
            .iter()
            .filter(|m| home.is_none_or(|mid| m.id == mid));
        for module in modules {
            walk_module(ws, module, bodies, filter, text_of, &mut |occ| {
                let Some(site) = occ.site else {
                    return;
                };
                let matches = match (function, occ.target) {
                    (Some((name, owned_only)), Self::Function(fmid, fid)) => {
                        let callee = ws.module(fmid);
                        let (same, home) = if occ.by_name {
                            (callee.functions[fid].name == name, module)
                        } else if scope == FnRefScope::AllOverloads {
                            (callee.functions[fid].name == name, callee)
                        } else {
                            (occ.target == self, callee)
                        };
                        same && !(owned_only && is_read_only_source(home.file.as_str()))
                    }
                    (_, target) => target == self,
                };
                if matches && seen.insert((site.span.file, site.span.offset_range())) {
                    sites.push(site);
                }
            });
        }
        sites.sort_by_key(|site| (site.span.file.as_str(), site.span.offset_range()));
        sites
    }
}
