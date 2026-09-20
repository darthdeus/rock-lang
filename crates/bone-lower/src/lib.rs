
pub mod resolve;

use skull_parser::{
    Annotation, Ast, Binding, BlockData, DeferKind, ExprData, ExprKind, FunctionDef, IfLetClause, StatementData, StatementKind, TokenKind, TypeExprData
};
use ustr::ustr;

use bone_core::{
    annotations::{validate_annotations, AnnoPos},
    hir::{
        AsKind, BindingInfo, BindingKind, BindingPattern, Callee, DecoratorDef, EnumVariantDef,
        Expr, Function, GlobalDef, InterpPart, InterpSpec, MatchArm, ParamDecl, Pattern,
        RecordPatField, Scope, StateMetadata, StateOwnership, StateRealization,
        StateWorldPolicy, Statement, StructLitField, TyId, TypeCtor, TypeExpr, VarId,
        VariantArgs, VariantPayload,
    },
    prelude::*,
};

#[derive(Copy, Clone, Debug)]
pub enum TyPos {
    /// Interface position (signatures, fields, globals, enum/newtype):
    /// module ty_arena, TyId::Static - what other modules may reference
    Iface,
    /// Body position (let / as / ascription / lambda annotations):
    /// Body.lowered_tys, TyId::Local - dies with the body
    Body,
}

/// Record how each segment of a v1 fragment's top-level `use` items resolved
/// ([`hir::PathSegmentRef`]), on the module; `use a::f as g` also records
/// `g` as an alias of what `f` names. The targets are checked where they are
/// declared (`validate_v1_use_targets`), so a segment that does not resolve
/// here records nothing rather than a second diagnostic. `use` items inside
/// inline `mod` blocks are not recorded yet.
pub fn record_use_paths(a: &Ast, ctx: &mut LowerCtx) {
    let mut refs = Vec::new();
    for item in &a.items {
        if let skull_parser::ItemKind::Use(decl) = &item.kind {
            ctx.use_tree_refs(&decl.tree, &[], false, &mut refs);
        }
    }
    ctx.module.path_segments.extend(refs);
}

/// The segment record for what a member-wise path resolved to.
fn member_segment_target(target: &resolve::MemberTarget) -> hir::SegmentTarget {
    match *target {
        resolve::MemberTarget::Global(key)
        | resolve::MemberTarget::Callable { key, .. }
        | resolve::MemberTarget::Type(key) => hir::SegmentTarget::Def(key),
        resolve::MemberTarget::EnumVariant { enum_key, variant } => hir::SegmentTarget::Variant { enum_key, variant },
    }
}

pub fn alloc_ty(ctx: &mut LowerCtx, body: &mut Body, pos: TyPos, node: TypeExpr) -> TyId {
    match pos {
        TyPos::Iface => TyId::Static(ctx.module.module_id(), alloc_ty_raw(ctx, body, pos, node)),
        TyPos::Body => TyId::Local(alloc_ty_raw(ctx, body, pos, node)),
    }
}

pub fn alloc_ty_raw(
    ctx: &mut LowerCtx,
    body: &mut Body,
    pos: TyPos,
    node: TypeExpr,
) -> Idx<TypeExpr> {
    match pos {
        TyPos::Iface => ctx.module.ty_arena.alloc(node),
        TyPos::Body => body.lowered_tys.alloc(node),
    }
}

pub fn alloc_tys_contiguous(
    ctx: &mut LowerCtx,
    body: &mut Body,
    pos: TyPos,
    nodes: impl IntoIterator<Item = TypeExpr>,
) -> IdxRange<TypeExpr> {
    match pos {
        TyPos::Iface => alloc_contiguous(&mut ctx.module.ty_arena, nodes),
        TyPos::Body => alloc_contiguous(&mut body.lowered_tys, nodes),
    }
}

#[derive(Debug)]
pub struct LowerCtx<'a> {
    pub workspace: &'a Workspace,
    pub module: &'a mut hir::Module,
    pub generics_in_scope: FxHashMap<Ustr, TyId>,

    pub _next_var_id: u32,

    pub placeholder_param: Option<Idx<BindingInfo>>,
    pub placeholder_forbidden: bool,

    /// The namespace facts this lowering's lookups read (plan 2.6), moved
    /// into `Module.scope_probes` when the lowering finishes
    /// ([`LowerCtx::flush_scope_probes`]). A cell because lookups run from
    /// `&self` helpers.
    pub scope_probes: std::cell::RefCell<Vec<bone_core::modules::ScopeProbe>>,
}

impl<'a> LowerCtx<'a> {
    /// Run one resolver lookup and record what it read.
    fn probed<R>(&self, lookup: impl FnOnce() -> R) -> R {
        let (result, probes) = resolve::with_probes(lookup);
        self.scope_probes.borrow_mut().extend(probes);
        result
    }

    /// A legacy file's bare-name lookup reads `legacy_name(file, name)`.
    fn probe_legacy(&self, name: Ustr) {
        self.scope_probes
            .borrow_mut()
            .push(bone_core::modules::ScopeProbe::Legacy { name });
    }

    /// Move the recorded probes into the module being lowered.
    pub fn flush_scope_probes(&mut self) {
        let probes = std::mem::take(self.scope_probes.get_mut());
        self.module.scope_probes.extend(probes);
    }

    /// This file's modules-v1 fragment scope (None for legacy files and
    /// synthetic modules). Resolution tiering applies only when present.
    pub fn v1_scope(&self) -> Option<&bone_core::modules::FragmentScope> {
        self.workspace.fragment_scopes.get(&self.module.file)
    }

    /// How each segment of the v1 `path` resolved, `last` being what the
    /// whole path resolved to ([`hir::PathSegmentRef`]). A segment before the
    /// last is the module the resolver's own walk reaches through it -- or,
    /// right before a variant, the variant's enum. A prefix that reaches no
    /// module (a type) records nothing. The walks are probed like the lookup
    /// itself, so a tree edit that moves a segment re-lowers this module.
    fn path_segment_refs(
        &self,
        from_root: bool,
        segments: &[skull_parser::Ident],
        last: hir::SegmentTarget,
    ) -> Vec<hir::PathSegmentRef> {
        let Some(frag) = self.v1_scope() else { return Vec::new() };
        let names: Vec<Ustr> = segments.iter().map(|i| i.name).collect();
        let n = segments.len();
        let mut out = Vec::with_capacity(n);
        for (i, ident) in segments.iter().enumerate() {
            let target = if i + 1 == n {
                Some(last)
            } else if let (hir::SegmentTarget::Variant { enum_key, .. }, true) = (last, i + 2 == n) {
                Some(hir::SegmentTarget::Def(enum_key))
            } else {
                self.module_segment(frag, from_root, &names[..=i])
            };
            if let Some(target) = target {
                out.push(hir::PathSegmentRef { span: ident.span, target, alias: false });
            }
        }
        out
    }

    /// The module a path prefix walks to, as the resolver walks it.
    fn module_segment(
        &self,
        frag: &bone_core::modules::FragmentScope,
        from_root: bool,
        prefix: &[Ustr],
    ) -> Option<hir::SegmentTarget> {
        let prefix = bone_core::modules::UsePathRef { from_root, segments: prefix.to_vec() };
        self.probed(|| resolve::resolve_path_to_module(self.workspace, frag, &prefix))
            .ok()
            .map(|(package, path)| hir::SegmentTarget::Module {
                package,
                node: bone_core::modules::NsPackage::path_key(&path),
            })
    }

    /// Record `path`'s segments in the body (a body position) or the module
    /// (an interface position).
    fn record_path(&mut self, body: &mut Body, pos: TyPos, path: &skull_parser::Path, last: hir::SegmentTarget) {
        let refs = self.path_segment_refs(path.from_root, &path.segments, last);
        match pos {
            TyPos::Body => body.path_segments.extend(refs),
            TyPos::Iface => self.module.path_segments.extend(refs),
        }
    }

    /// [`record_use_paths`] for one use tree under `prefix`.
    fn use_tree_refs(
        &self,
        tree: &skull_parser::UseTree,
        prefix: &[Ustr],
        from_root: bool,
        out: &mut Vec<hir::PathSegmentRef>,
    ) {
        let Some(frag) = self.v1_scope() else { return };
        let skull_parser::UseTree::Path { from_root: tree_root, segments, kind } = tree;
        let root = if prefix.is_empty() { *tree_root } else { from_root };
        let mut full = prefix.to_vec();
        full.extend(segments.iter().map(|s| s.name));
        let binds_item = matches!(kind, skull_parser::UseSuffix::Bind | skull_parser::UseSuffix::BindAs(_));
        for (i, ident) in segments.iter().enumerate() {
            let upto = prefix.len() + i + 1;
            let target = if binds_item && upto == full.len() {
                self.use_leaf_target(frag, root, &full)
            } else {
                self.module_segment(frag, root, &full[..upto])
            };
            if let Some(target) = target {
                out.push(hir::PathSegmentRef { span: ident.span, target, alias: false });
            }
        }
        match kind {
            skull_parser::UseSuffix::BindAs(alias) => {
                if let Some(target) = self.use_leaf_target(frag, root, &full) {
                    out.push(hir::PathSegmentRef { span: alias.span, target, alias: true });
                }
            }
            skull_parser::UseSuffix::Nested(children) => {
                for child in children {
                    self.use_tree_refs(child, &full, root, out);
                }
            }
            skull_parser::UseSuffix::Bind | skull_parser::UseSuffix::Glob | skull_parser::UseSuffix::Operator { .. } => {}
        }
    }

    /// What a `use` leaf binds: a definition, else a module (`use p;`).
    fn use_leaf_target(
        &self,
        frag: &bone_core::modules::FragmentScope,
        from_root: bool,
        segments: &[Ustr],
    ) -> Option<hir::SegmentTarget> {
        let path = bone_core::modules::UsePathRef { from_root, segments: segments.to_vec() };
        match self.probed(|| resolve::resolve_path(self.workspace, frag, &path)) {
            Ok(resolved) => Some(hir::SegmentTarget::Def(resolved.key)),
            Err(_) => self.module_segment(frag, from_root, segments),
        }
    }

    fn resolve_bare_key(&self, name: Ustr, span: Span) -> Result<Ustr, CompilerError> {
        Ok(self.resolve_bare(name, span)?.key)
    }

    /// The enum the base of a qualified variant (`E::V`, `E::V(..)`,
    /// `E::V { .. }`, and their patterns) names, by the key the workspace
    /// maps use: the base resolves like any bare name, so a legacy file
    /// reaches a projected v1 enum by its DefKey (spec 3.2). `None` when the
    /// base names no enum.
    fn variant_enum_key(&self, base: Ustr, span: Span) -> Result<Option<Ustr>, CompilerError> {
        let key = self.resolve_bare_key(base, span)?;
        Ok(self.workspace.enums_by_name.contains_key(&key).then_some(key))
    }

    /// The DefKey of a v1 path that is CALLED BY NAME -- a callable family, or a
    /// newtype, whose constructor is a call on the type's own name and which the
    /// bare-name route already resolves this way -- with the view owning a
    /// legacy family's key. `None` for a legacy fragment, a path that does not
    /// resolve, or one that names a struct, enum, global or enum variant --
    /// those keep the general call route.
    fn v1_path_callable(&self, path: &skull_parser::Path) -> Option<(Ustr, Option<bone_core::modules::PackageId>)> {
        let frag = self.v1_scope()?;
        let use_path = bone_core::modules::UsePathRef {
            from_root: path.from_root,
            segments: path.segments.iter().map(|i| i.name).collect(),
        };
        match self.probed(|| resolve::resolve_member_path(self.workspace, frag, &use_path)) {
            Ok(resolve::MemberTarget::Callable { key, owner }) => Some((key, owner)),
            Ok(resolve::MemberTarget::Global(key))
                if matches!(
                    self.workspace.defs.get(&key).map(|m| m.kind),
                    Some(bone_core::modules::DefKind::Callable)
                ) =>
            {
                Some((key, None))
            }
            // `p::NT(x)`. As a VALUE the path is the type, and the call then
            // lowers as a call of a value: inference reports the key as an
            // undefined global, because a newtype constructor is resolved by
            // name, exactly as `NT(x)` is inside the defining module.
            Ok(resolve::MemberTarget::Type(key))
                if self.workspace.newtypes_by_name.contains_key(&key) =>
            {
                Some((key, None))
            }
            _ => None,
        }
    }

    /// The `(enum DefKey, variant)` of a v1 path that names an enum VARIANT.
    /// `None` for a legacy fragment or a path that names anything else.
    fn v1_path_variant(&self, path: &skull_parser::Path) -> Option<(Ustr, Ustr)> {
        let frag = self.v1_scope()?;
        let use_path = bone_core::modules::UsePathRef {
            from_root: path.from_root,
            segments: path.segments.iter().map(|i| i.name).collect(),
        };
        match self.probed(|| resolve::resolve_member_path(self.workspace, frag, &use_path)) {
            Ok(resolve::MemberTarget::EnumVariant { enum_key, variant }) => {
                Some((enum_key, variant))
            }
            _ => None,
        }
    }

    fn resolve_bare(
        &self,
        name: Ustr,
        span: Span,
    ) -> Result<resolve::Resolved, CompilerError> {
        match self.v1_scope() {
            Some(frag) => self.probed(|| resolve::resolve_bare(self.workspace, Some(frag), name)),
            None => {
                self.probe_legacy(name);
                resolve::resolve_legacy_bare(self.workspace, self.module.file, name)
            }
        }
        .map_err(|msg| CompilerError::new_with_label(span, msg))
    }

    /// A bare name in callee position: the spelling the callee carries and
    /// its overload family. v1 goes through the tiers (spec 8.3); a legacy
    /// name goes through the one legacy lookup (spec 3.2). A family spanning
    /// several definitions keeps the bare name as its spelling -- which is
    /// also what lets scalar operands of a merged operator lower natively --
    /// and records its member keys on the module for inference.
    fn resolve_callee_family(
        &mut self,
        name: Ustr,
        span: Span,
    ) -> Result<(Ustr, hir::CallFamily), CompilerError> {
        let file = self.module.file;
        let resolved = match self.v1_scope() {
            Some(frag) => self.probed(|| resolve::resolve_v1_callee(self.workspace, frag, name)),
            None => {
                self.probe_legacy(name);
                resolve::resolve_legacy_callee(self.workspace, file, name)
            }
        }
        .map_err(|msg| CompilerError::new_with_label(span, msg))?;
        Ok(self.record_family(name, resolved))
    }

    /// The family of a unary operator's function (`-x` -> `__unm`, `!x` ->
    /// `__not`), resolved like a binary operator's callee. In v1 a name no
    /// tier binds is a family with no members rather than an error: the
    /// operand may be a scalar, which never consults it.
    fn resolve_unary_family(&mut self, name: Ustr, span: Span) -> Result<hir::CallFamily, CompilerError> {
        Ok(self.resolve_optional_family(name, span)?.1)
    }

    /// The family of `recv.name(..)`. In v1 a name no tier binds is a family
    /// with no members rather than an error: a fn-typed field `name` on the
    /// receiver is called first (spec 8.4 keeps that precedence), and
    /// inference reports the call when there is neither.
    fn resolve_method_family(&mut self, name: Ustr, span: Span) -> Result<(Ustr, hir::CallFamily), CompilerError> {
        self.resolve_optional_family(name, span)
    }

    fn resolve_optional_family(&mut self, name: Ustr, span: Span) -> Result<(Ustr, hir::CallFamily), CompilerError> {
        let file = self.module.file;
        let resolved = match self.v1_scope() {
            Some(frag) => self.probed(|| resolve::resolve_v1_unary(self.workspace, frag, name)),
            None => {
                self.probe_legacy(name);
                resolve::resolve_legacy_callee(self.workspace, file, name).map(Some)
            }
        }
        .map_err(|msg| CompilerError::new_with_label(span, msg))?;
        let resolved = resolved.unwrap_or(resolve::CalleeFamily::Merged { members: Vec::new(), hidden: Vec::new() });
        Ok(self.record_family(name, resolved))
    }

    /// The spelling and `CallFamily` of a resolved callee; a merged family's
    /// member keys go on the module for inference.
    fn record_family(&mut self, name: Ustr, resolved: resolve::CalleeFamily) -> (Ustr, hir::CallFamily) {
        let file = self.module.file;
        match resolved {
            resolve::CalleeFamily::Single(r) if self.v1_scope().is_some() => {
                (r.key, hir::CallFamily::resolved(r.key, r.owner, &r.hidden))
            }
            resolve::CalleeFamily::Single(r) => (r.key, hir::CallFamily::from_name(r.key)),
            resolve::CalleeFamily::Merged { members, hidden } => {
                self.module.call_families.insert(name, members);
                (name, hir::CallFamily::merged(name, file, &hidden))
            }
        }
    }

    pub fn next_var_id(&mut self) -> VarId {
        self._next_var_id += 1;
        VarId(self._next_var_id)
    }

    pub fn alloc_binding(
        &mut self,
        body: &mut Body,
        b: BindingInfo,
        span: Span,
    ) -> Idx<BindingInfo> {
        let idx = body.binding_arena.alloc(b);

        body.binding_spans.insert(idx, span);
        body.make_lapper(
            span.offset_range.0,
            span.offset_range.1,
            hir::NodeRef::Binding(idx),
        );

        idx
    }

    pub fn alloc_expr(&mut self, body: &mut Body, expr: Expr, span: Span) -> Idx<Expr> {
        let is_block = matches!(expr, Expr::Block { .. });
        let idx = body.expr_arena.alloc(expr);

        body.expr_spans.insert(idx, span);

        if !is_block {
            body.make_lapper(
                span.offset_range.0,
                span.offset_range.1,
                hir::NodeRef::Expr(idx),
            );
        }

        idx
    }

    pub fn alloc_block_expr(&mut self, body: &mut Body, expr: Expr, b: &BlockData) -> Idx<Expr> {
        debug_assert!(matches!(expr, Expr::Block { .. }));
        let idx = body.expr_arena.alloc(expr);

        body.expr_spans.insert(idx, b.span);

        if let Some((open, close)) = b.braces {
            body.make_lapper(
                open.offset_range.0,
                open.offset_range.1,
                hir::NodeRef::Expr(idx),
            );

            body.make_lapper(
                close.offset_range.0,
                close.offset_range.1,
                hir::NodeRef::Expr(idx),
            );
        }

        idx
    }

    pub fn bind_let(
        &mut self,
        body: &mut Body,
        name: Ustr,
        span: Span,
        scope: Idx<Scope>,
    ) -> Idx<BindingInfo> {
        let var = self.alloc_binding(
            body,
            BindingInfo {
                name,
                kind: hir::BindingKind::Let,
                declared_ty: None,
                span,
            },
            span,
        );

        body.scope_arena[scope].bindings.insert(name, var);
        var
    }

    pub fn lower_pattern_bindings(
        &mut self,
        body: &mut Body,
        binding: &Binding,
        scope: Idx<Scope>,
    ) -> Result<BindingPattern, CompilerError> {
        match &binding {
            Binding::Ident(id) => Ok(BindingPattern::Leaf(
                self.bind_let(body, id.name, id.span, scope),
            )),

            Binding::Tuple(t) => {
                let mut subs = Vec::with_capacity(t.len());

                for b in t.iter() {
                    // recursion
                    subs.push(self.lower_pattern_bindings(body, b, scope)?);
                }

                Ok(BindingPattern::Tuple(subs))
            }

            Binding::Newtype { binding: nb, .. } => Ok(BindingPattern::Leaf(
                self.bind_let(body, nb.name, nb.span, scope),
            )),
        }
    }

    pub fn lower_type_exprs_to_range(
        &mut self,
        a: &Ast,
        body: &mut Body,
        pos: TyPos,
        xs: &[Idx<TypeExprData>],
        span: Span,
    ) -> Result<IdxRange<TypeExpr>, CompilerError> {
        let nodes: Vec<TypeExpr> = xs
            .iter()
            .map(|x| {
                let id = lower_type_expr_in(self, a, body, pos, &a.types[*x])?;
                self.type_node(body, id, span)
            })
            .collect::<Result<_, _>>()?;

        Ok(alloc_tys_contiguous(self, body, pos, nodes))
    }

    pub fn type_node(&self, body: &Body, ty: TyId, _span: Span) -> Result<TypeExpr, CompilerError> {
        match ty {
            TyId::Static(m, i) if m == self.module.module_id() => Ok(self.module.ty_arena[i]),
            TyId::Static(m, i) => Ok(self.workspace.module(m).ty_arena[i]),
            TyId::Local(i) => Ok(body.lowered_tys[i]),
        }
    }

    pub fn declare_generics(&mut self, generics: &[skull_parser::Generic]) -> Vec<TyId> {
        self.generics_in_scope.clear();

        generics
            .iter()
            .map(|g| {
                let var_id = self.next_var_id();
                let ty_idx = self
                    .module
                    .ty_arena
                    .alloc(hir::TypeExpr::Generic(var_id, g.name.name));

                self.module.ty_spans.insert(ty_idx, g.name.span);

                let tyid = TyId::Static(self.module.module_id(), ty_idx);

                self.generics_in_scope.insert(g.name.name, tyid);
                tyid
            })
            .collect()
    }
}

pub fn lower_type_expr(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    te: &skull_parser::TypeExprData,
) -> Result<TyId, CompilerError> {
    lower_type_expr_in(ctx, a, body, TyPos::Iface, te)
}

pub fn lower_type_expr_local(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    te: &skull_parser::TypeExprData,
) -> Result<TyId, CompilerError> {
    lower_type_expr_in(ctx, a, body, TyPos::Body, te)
}

/// Whether a `Static` type's source span is safe to record in `module.ty_spans`
/// (keyed by the freshly-allocated `ty_arena` idx). True for every non-`Ident`
/// type and for `Ident`s that resolve to a nominal struct/enum/newtype -- those
/// get a fresh per-occurrence idx, so per-occurrence span recording is correct
/// and lets hover / goto-definition resolve `m: Mob` / `-> Vec2`. Skipped for
/// generics-in-scope and `_`, which resolve to a SHARED/pre-existing `TyId`
/// (recording would clobber another occurrence's span).
fn span_recordable(
    ctx: &LowerCtx,
    kind: &skull_parser::TypeExprKind,
    resolved_key: Option<&Ustr>,
) -> bool {
    use skull_parser::TypeExprKind;
    match kind {
        TypeExprKind::Ident(n) => {
            !ctx.generics_in_scope.contains_key(&n.name)
                && n.name.as_str() != "_"
                && resolved_key.is_some_and(|key| {
                    ctx.workspace.struct_by_name.contains_key(key)
                        || ctx.workspace.enums_by_name.contains_key(key)
                        || ctx.workspace.newtypes_by_name.contains_key(key)
                })
        }
        _ => true,
    }
}

/// Lower a nominal type reference by its resolved canonical key. Shared by
/// bare type names (after tier resolution) and qualified type paths.
fn lower_resolved_type_name(
    ctx: &mut LowerCtx,
    body: &mut Body,
    pos: TyPos,
    te: &skull_parser::TypeExprData,
    key: &Ustr,
) -> Result<TyId, CompilerError> {
    let is_intrinsic = bone_core::modules::intrinsic_type_name(*key).is_some();
    if !is_intrinsic
        && !(ctx.workspace.struct_by_name.contains_key(key)
            || ctx.workspace.enums_by_name.contains_key(key)
            || ctx.workspace.newtypes_by_name.contains_key(key))
    {
        let msg = format!("Unknown type name {}", key);
        match pos {
            TyPos::Iface => {
                ctx.module
                    .interface_errors
                    .push(CompilerError::new_with_label(te.span, msg));
                return Ok(ctx.workspace.builtins.error());
            }
            TyPos::Body => return Err(CompilerError::new_with_label(te.span, msg)),
        }
    }
    let generic_arity = ctx
        .workspace
        .type_generic_arity
        .get(key)
        .copied()
        .or_else(|| is_intrinsic.then_some(2))
        .unwrap_or(0);
    let defaults = generic_defaults(ctx.workspace, *key);
    let min_arity = generic_arity - defaults.len();

    if min_arity > 0 {
        let msg = if defaults.is_empty() {
            format!("Generic type `{}` requires {} type argument(s)", key, generic_arity)
        } else {
            format!("Generic type `{}` requires at least {} type argument(s)", key, min_arity)
        };
        match pos {
            TyPos::Iface => {
                ctx.module.interface_errors.push(CompilerError::new_with_label(te.span, msg));
                return Ok(ctx.workspace.builtins.error());
            }
            TyPos::Body => return Err(CompilerError::new_with_label(te.span, msg)),
        }
    }

    let params = alloc_tys_contiguous(
        ctx,
        body,
        pos,
        defaults.iter().map(|d| default_type_expr(*d)).collect::<Vec<_>>(),
    );

    Ok(alloc_ty(
        ctx,
        body,
        pos,
        TypeExpr::App {
            ctor: hir::TypeCtor::Nominal(*key),
            params,
        },
    ))
}

pub fn lower_type_expr_in(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    pos: TyPos,
    te: &skull_parser::TypeExprData,
) -> Result<TyId, CompilerError> {
    use skull_parser::TypeExprKind;

    let mut resolved_key = None;
    let idx = match &te.kind {
        TypeExprKind::Int => ctx.workspace.builtins.int(),
        TypeExprKind::Uint => ctx.workspace.builtins.uint(),
        TypeExprKind::Float => ctx.workspace.builtins.float(),
        TypeExprKind::Bool => ctx.workspace.builtins.bool(),
        TypeExprKind::String => ctx.workspace.builtins.string(),
        TypeExprKind::Unit => ctx.workspace.builtins.unit(),
        TypeExprKind::Any => ctx.workspace.builtins.any(),

        // Modules-v1 qualified type paths resolve through the namespace tree
        // (plan M2) to a canonical DefKey; everything downstream keys on it.
        TypeExprKind::Path(path) => {
            let Some(frag) = ctx.v1_scope() else {
                return Err(CompilerError::new_with_label(
                    te.span,
                    format!(
                        "qualified type path `{}` is modules-v1 grammar; this file is legacy",
                        path.segments.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join("::")
                    ),
                ));
            };
            let use_path = bone_core::modules::UsePathRef {
                from_root: path.from_root,
                segments: path.segments.iter().map(|i| i.name).collect(),
            };
            let resolved = match ctx.probed(|| resolve::resolve_path(ctx.workspace, frag, &use_path)) {
                Ok(r) => r,
                Err(msg) => return Err(CompilerError::new_with_label(te.span, msg)),
            };
            ctx.record_path(body, pos, path, hir::SegmentTarget::Def(resolved.key));
            lower_resolved_type_name(ctx, body, pos, te, &resolved.key)?
        }

        TypeExprKind::AppPath { ctor: path, args } => {
            let Some(frag) = ctx.v1_scope() else {
                return Err(CompilerError::new_with_label(
                    te.span,
                    format!(
                        "qualified type path `{}` is modules-v1 grammar; this file is legacy",
                        path.segments.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join("::")
                    ),
                ));
            };
            let use_path = bone_core::modules::UsePathRef {
                from_root: path.from_root,
                segments: path.segments.iter().map(|i| i.name).collect(),
            };
            let resolved = match ctx.probed(|| resolve::resolve_path(ctx.workspace, frag, &use_path)) {
                Ok(r) => r,
                Err(msg) => return Err(CompilerError::new_with_label(te.span, msg)),
            };
            ctx.record_path(body, pos, path, hir::SegmentTarget::Def(resolved.key));
            let params = ctx.lower_type_exprs_to_range(a, body, pos, args, te.span)?;
            alloc_ty(
                ctx,
                body,
                pos,
                TypeExpr::App {
                    ctor: hir::TypeCtor::for_name(resolved.key),
                    params,
                },
            )
        }

        TypeExprKind::Ident(name) => {
            if name.name.as_str() == "_" {
                let var = ctx.next_var_id();
                alloc_ty(ctx, body, pos, TypeExpr::Infer(var, name.name))
            } else if let Some(g) = ctx.generics_in_scope.get(&name.name) {
                *g
            } else {
                let bare_key = ctx.resolve_bare_key(name.name, name.span)?;
                resolved_key = Some(bare_key);
                lower_resolved_type_name(ctx, body, pos, te, &bare_key)?
            }
        }

        TypeExprKind::Tuple(xs) => {
            let elements = ctx.lower_type_exprs_to_range(a, body, pos, xs, te.span)?;
            alloc_ty(ctx, body, pos, hir::TypeExpr::Tuple { elements })
        }

        TypeExprKind::Array(orig_te) => {
            // TODO: recursion breaks here too maybe?
            let te = lower_type_expr_helper(ctx, a, body, pos, a.te(orig_te))?;

            let arr = alloc_tys_contiguous(ctx, body, pos, [te]);

            alloc_ty(
                ctx,
                body,
                pos,
                hir::TypeExpr::App {
                    ctor: hir::TypeCtor::Array,
                    params: arr,
                },
            )
        }

        TypeExprKind::Ref(inner) => {
            let inner = lower_type_expr_helper(ctx, a, body, pos, a.te(inner))?;
            let params = alloc_tys_contiguous(ctx, body, pos, [inner]);
            alloc_ty(
                ctx,
                body,
                pos,
                TypeExpr::App {
                    ctor: TypeCtor::Ref,
                    params,
                },
            )
        }

        TypeExprKind::App { ctor, args } => {
            let resolved_ctor = ctx.resolve_bare_key(ctor.name, ctor.span)?;
            let is_field_ref = ["FieldRef", "ReadFieldRef", "WriteFieldRef"]
                .iter()
                .any(|name| bone_core::modules::is_intrinsic_key(resolved_ctor, name));

            if is_field_ref {
                if args.len() != 2 {
                    comp_error!(
                        te.span,
                        "{} type must have exactly 2 type arguments (<Struct, FieldType>)",
                        ctor.name
                    );
                }

                let t_te = lower_type_expr_helper(ctx, a, body, pos, a.te(&args[0]))?;
                let v_te = lower_type_expr_helper(ctx, a, body, pos, a.te(&args[1]))?;

                let arr = alloc_tys_contiguous(ctx, body, pos, [t_te, v_te]);

                alloc_ty(
                    ctx,
                    body,
                    pos,
                    TypeExpr::App {
                        ctor: TypeCtor::Nominal(resolved_ctor),
                        params: arr,
                    },
                )
            } else {
                let type_key = resolved_ctor;
                let mut lowered = args
                    .iter()
                    .map(|x| lower_type_expr_helper(ctx, a, body, pos, a.te(x)))
                    .try_vec()?;

                // An exact bare Map key is compiler-owned. In legacy source
                // there is no namespace key to distinguish it from a user
                // declaration, so a workspace-wide same-named type must not
                // change stdlib lowering.
                let is_map = bone_core::modules::is_intrinsic_key(type_key, "Map");
                if is_map && args.len() != 2 {
                    comp_error!(te.span, "Map type must have exactly 2 type arguments");
                }

                if bone_core::modules::is_intrinsic_key(type_key, "Coroutine") {
                    // Builtin dual-arity Coroutine<Y[, R[, N]]>: same
                    // fill-from-right mechanism as user defaults, all slots Unit.
                    while lowered.len() < 3 {
                        lowered.push(TypeExpr::Builtin(hir::BuiltinTy::Unit));
                    }
                }

                // Generic-argument count for a user nominal. Trailing defaulted
                // params (workspace map, built pre-lowering) fill from the right:
                // `Pair<string>` on `struct Pair<T, U = int>` pads to
                // `Pair<string, int>`. Builtins (Coroutine, Map, Vec/Array) aren't
                // in `type_generic_arity`, so they keep their own arity logic.
                // Cross-module via the workspace map. The bare (0-arg) case is
                // handled in the Ident arm.
                if !is_map {
                    if let Some(&arity) = ctx.workspace.type_generic_arity.get(&type_key) {
                        let defaults = generic_defaults(ctx.workspace, type_key);
                        let min = arity - defaults.len();
                        if args.len() < min || args.len() > arity {
                            if defaults.is_empty() {
                                comp_error!(
                                    te.span,
                                    "Generic type `{}` expects {} type argument(s), found {}",
                                    type_key,
                                    arity,
                                    args.len()
                                );
                            }
                            comp_error!(
                                te.span,
                                "Generic type `{}` expects {} to {} type argument(s), found {}",
                                type_key,
                                min,
                                arity,
                                args.len()
                            );
                        }
                        while lowered.len() < arity {
                            lowered.push(default_type_expr(defaults[lowered.len() - min]));
                        }
                    }
                }

                let params = alloc_tys_contiguous(ctx, body, pos, lowered);

                alloc_ty(
                    ctx,
                    body,
                    pos,
                    TypeExpr::App {
                        ctor: TypeCtor::for_name(type_key),
                        params,
                    },
                )
            }
        }

        TypeExprKind::Function { params, ret } => {
            let lowered = params
                .iter()
                .map(|p| lower_type_expr_helper(ctx, a, body, pos, a.te(p)))
                .try_vec()?;

            let params_range = alloc_tys_contiguous(ctx, body, pos, lowered);

            let ret_te = lower_type_expr_helper(ctx, a, body, pos, a.te(ret))?;
            let ret_idx = alloc_ty_raw(ctx, body, pos, ret_te);

            alloc_ty(
                ctx,
                body,
                pos,
                TypeExpr::Function {
                    params: params_range,
                    ret: ret_idx,
                },
            )
        }

        TypeExprKind::Hole(_, name) => {
            let hint = name.unwrap_or_else(|| ustr("_"));
            let var = ctx.next_var_id();
            alloc_ty(ctx, body, pos, TypeExpr::Infer(var, hint))
        }
    };

    let idx = if te.is_nullable {
        let inner_te = ctx.type_node(body, idx, te.span)?;
        let inner = alloc_ty_raw(ctx, body, pos, inner_te);

        alloc_ty(ctx, body, pos, TypeExpr::Nullable(inner))
    } else {
        idx
    };

    match idx {
        TyId::Static(mid, tidx) => {
            if mid != ctx.workspace.builtin_module().module_id() {
                if mid != ctx.module.module_id() {
                    comp_error!(te.span, "Lowered expr got a different module id than current module during lowering ... expected: {:?} ... actual {:?}", ctx.module.module_id(), mid);
                } else if span_recordable(ctx, &te.kind, resolved_key.as_ref()) {
                    ctx.module.ty_spans.insert(tidx, te.span);
                }
            }

            body.make_lapper(
                te.span.offset_range.0,
                te.span.offset_range.1,
                hir::NodeRef::StaticTy { mid, idx: tidx },
            );
        }

        TyId::Local(tidx) => {
            body.lowered_ty_spans.insert(tidx, te.span);
            body.make_lapper(
                te.span.offset_range.0,
                te.span.offset_range.1,
                hir::NodeRef::LocalTy(tidx),
            );
        }
    }

    Ok(idx)
}

// TODO: maybe just return &Expr?
pub fn lookup_in_scope(
    ctx: &LowerCtx,
    body: &mut Body,
    start_scope: Idx<Scope>,
    name: &skull_parser::Ident,
) -> Result<Expr, CompilerError> {
    if let Some(binding) = lookup_local(body, start_scope, name.name) {
        return Ok(Expr::Var(binding));
    }
    // Tier 1 misses: a v1 fragment resolves through the namespace tiers to a
    // canonical key; legacy files keep the bare name.
    let key = ctx.resolve_bare_key(name.name, name.span)?;
    Ok(Expr::Global(key))
}

/// The lexical binding `name` names from `start_scope`, if any (tier 1).
fn lookup_local(body: &Body, start_scope: Idx<Scope>, name: Ustr) -> Option<Idx<BindingInfo>> {
    let mut scope = start_scope;
    loop {
        if let Some(&binding) = body.scope_arena[scope].bindings.get(&name) {
            return Some(binding);
        }
        scope = body.scope_arena[scope].parent?;
    }
}

pub fn lower_block(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    parent: Idx<Scope>,
    block: &Idx<BlockData>,
) -> Result<Expr, CompilerError> {
    let scope = body.alloc_child_scope(parent);

    let (statements, tail) = lower_statements(ctx, a, body, scope, &a.b(block).statements)?;

    Ok(Expr::Block { statements, tail })
}

pub fn lower_statement_value(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    scope: Idx<Scope>,
    s: &StatementData,
) -> Result<Statement, CompilerError> {
    let hir_s = match &s.kind {
        StatementKind::Use(u) => comp_error!(
            u.span,
            "`use` declarations require module resolution (not yet implemented)"
        ),
        StatementKind::Expression(expr) => {
            Statement::Expr(lower_expr(ctx, a, body, scope, a.e(expr))?)
        }

        StatementKind::Return {
            keyword_span,
            value: ret,
        } => {
            if let Some(expr) = &ret {
                let e_id = lower_expr(ctx, a, body, scope, a.e(expr))?;

                if let Some(kw_span) = keyword_span {
                    body.make_lapper(
                        kw_span.offset_range.0,
                        kw_span.offset_range.1,
                        // TODO: statement id?
                        hir::NodeRef::Expr(e_id),
                    );
                } else {
                    // TODO: when?
                }

                Statement::Return(Some(e_id))
            } else {
                Statement::Return(None)
            }
        }

        StatementKind::Break { .. } => Statement::Break,
        StatementKind::Continue { .. } => Statement::Continue,

        StatementKind::Comment(text) => Statement::Comment(*text),

        StatementKind::Let(l) => {
            let value_idx = if let Some(e) = &l.value {
                lower_expr(ctx, a, body, scope, a.e(e))?
            } else {
                comp_error!(l.span, "let without value is not supported");
            };

            let declared_ty = if let Some(te) = &l.ty {
                Some(lower_type_expr_local(ctx, a, body, a.te(te))?)
            } else {
                None
            };

            match &l.binding {
                Binding::Ident(ident) => {
                    let binding = BindingInfo {
                        name: ident.name,
                        kind: hir::BindingKind::Let,
                        declared_ty,
                        span: ident.span,
                    };

                    let binding = ctx.alloc_binding(body, binding, ident.span);

                    body.scope_arena[scope].bindings.insert(ident.name, binding);

                    Statement::Let {
                        name: binding,
                        value: value_idx,
                    }
                }

                Binding::Tuple(..) => {
                    let vars = ctx.lower_pattern_bindings(body, &l.binding, scope)?;
                    Statement::LetTuple {
                        vars,
                        value: value_idx,
                    }
                }

                Binding::Newtype { name, binding } => {
                    let var = ctx.bind_let(body, binding.name, binding.span, scope);
                    let key = ctx.resolve_bare_key(name.name, name.span)?;

                    Statement::LetNewtype {
                        newtype_name: key,
                        var,
                        value: value_idx,
                    }
                }
            }
        }

        StatementKind::Static(st) => {
            let value_idx = lower_expr(ctx, a, body, scope, a.e(&st.value))?;

            let declared_ty = if let Some(te) = &st.ty {
                Some(lower_type_expr_local(ctx, a, body, a.te(te))?)
            } else {
                None
            };

            let binding = BindingInfo {
                name: st.name.name,
                kind: BindingKind::Static,
                declared_ty,
                span: st.name.span,
            };

            let binding = ctx.alloc_binding(body, binding, st.name.span);
            body.scope_arena[scope].bindings.insert(st.name.name, binding);

            Statement::Static { name: binding, value: value_idx }
        }

        StatementKind::NestedFn(f) => {
            // Name binds in the ENCLOSING scope, BEFORE the body is lowered, so a
            // self-recursive call resolves; a not-yet-declared sibling stays a
            // Global (decl-before-use -> mutual recursion rejected in inference).
            let name_binding = ctx.alloc_binding(
                body,
                BindingInfo {
                    name: f.name.name,
                    kind: BindingKind::Fn,
                    declared_ty: None,
                    span: f.name.span,
                },
                f.name.span,
            );
            body.scope_arena[scope].bindings.insert(f.name.name, name_binding);

            // Params live in a child scope whose parent is the enclosing scope,
            // so an enclosing param/local reference still resolves (then gets
            // rejected as a capture in inference), while the params shadow.
            let fn_scope = body.alloc_child_scope(scope);
            let mut binding_idxs = Vec::with_capacity(f.params.len());
            for p in &f.params {
                let declared_ty = Some(lower_type_expr_local(ctx, a, body, a.te(&p.ty))?);
                let b = ctx.alloc_binding(
                    body,
                    BindingInfo {
                        name: p.name.name,
                        kind: BindingKind::Param,
                        declared_ty,
                        span: p.name.span,
                    },
                    p.span,
                );
                body.scope_arena[fn_scope].bindings.insert(p.name.name, b);
                binding_idxs.push(b);
            }
            let params = match (binding_idxs.first(), binding_idxs.last()) {
                (Some(lo), Some(hi)) => IdxRange::new_inclusive(*lo..=*hi),
                _ => IdxRange::new_empty(),
            };

            let ret_ty = match &f.return_ty {
                Some(te) => Some(lower_type_expr_local(ctx, a, body, a.te(te))?),
                None => None,
            };

            let saved = ctx.placeholder_forbidden;
            ctx.placeholder_forbidden = true;
            let body_expr = lower_block(ctx, a, body, fn_scope, &f.body)?;
            ctx.placeholder_forbidden = saved;
            let body_idx = ctx.alloc_block_expr(body, body_expr, a.b(&f.body));

            Statement::Fn { name: name_binding, params, ret_ty, body: body_idx }
        }

        StatementKind::For(f) => {
            let iter = lower_expr(ctx, a, body, scope, a.e(&f.iter))?;

            let loop_scope = body.alloc_child_scope(scope);

            let vars = ctx.lower_pattern_bindings(body, &f.var, loop_scope)?;
            let body_expr = lower_block(ctx, a, body, loop_scope, &f.body)?;
            let body = ctx.alloc_block_expr(body, body_expr, a.b(&f.body));

            Statement::For { vars, iter, body }
        }

        StatementKind::While(w) => {
            let body_e = lower_block(ctx, a, body, scope, &w.body)?;
            let body_i = ctx.alloc_block_expr(body, body_e, a.b(&w.body));

            Statement::While {
                cond: lower_expr(ctx, a, body, scope, a.e(&w.condition))?,
                body: body_i,
            }
        }

        StatementKind::WhileLet(w) => {
            let value = lower_expr(ctx, a, body, scope, a.e(&w.value))?;

            let body_scope = body.alloc_child_scope(scope);

            let pat = lower_pattern_value(ctx, a, body, body_scope, &w.pattern)?;
            let pattern = body.pattern_arena.alloc(pat);

            let body_expr = lower_block(ctx, a, body, body_scope, &w.body)?;
            let body = ctx.alloc_block_expr(body, body_expr, a.b(&w.body));

            Statement::WhileLet {
                pattern,
                value,
                body,
            }
        }

        StatementKind::Defer(dk) => {
            let body = match dk {
                DeferKind::Block(b) => {
                    let e = lower_block(ctx, a, body, scope, b)?;
                    ctx.alloc_block_expr(body, e, a.b(b))
                }

                DeferKind::Expression(e) => lower_expr(ctx, a, body, scope, a.e(e))?,
            };

            Statement::Defer(body)
        }

        StatementKind::LifecycleHook {
            keyword_span,
            name,
            body: hook_body,
        } => {
            let name = name.as_str();
            let block = a.b(hook_body);

            if name == "on_killed" || name == "on_resume" || name == "on_hot_reload" {
                let hook_scope = body.alloc_child_scope(scope);
                let body_expr = lower_block(ctx, a, body, hook_scope, hook_body)?;
                let body_idx = ctx.alloc_block_expr(body, body_expr, block);

                let lambda = ctx.alloc_expr(
                    body,
                    Expr::Lambda {
                        params: IdxRange::new_empty(),
                        body: body_idx,
                    },
                    block.span,
                );

                let args = IdxRange::new_inclusive(lambda..=lambda);
                let call = ctx.alloc_expr(
                    body,
                    Expr::Call {
                        callee: Callee::Unresolved {
                            name: ustr(name),
                            name_span: keyword_span.unwrap_or(s.span),
                        },
                        args,
                        generic_args: IdxRange::new_empty(),
                        family: hir::CallFamily::from_name(ustr(name)),
                    },
                    s.span,
                );

                Statement::Expr(call)
            } else {
                let e = lower_block(ctx, a, body, scope, hook_body)?;
                Statement::Expr(ctx.alloc_expr(body, e, block.span))
            }
        }
    };

    Ok(hir_s)
}

pub fn lower_statement(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    scope: Idx<Scope>,
    s: &StatementData,
) -> Result<Idx<hir::Statement>, CompilerError> {
    let hir_s = lower_statement_value(ctx, a, body, scope, s)?;
    Ok(body.alloc_statement(hir_s, s.span))
}

pub fn lower_expr_value(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    scope: Idx<Scope>,
    e: &ExprData,
) -> Result<Expr, CompilerError> {
    use hir::Expr;
    use skull_parser::ExprKind;

    if !matches!(&e.kind, ExprKind::Lambda { .. } | ExprKind::Block(_))
        && is_placeholder_lambda(a, e)
    {
        if ctx.placeholder_forbidden {
            comp_error!(
                e.span,
                "`_` placeholder cannot appear inside an explicit lambda or block expression"
            );
        }

        if ctx.placeholder_param.is_none() {
            return lower_placeholder_lambda(ctx, a, body, scope, e);
        }
    }


    let result = match &e.kind {
        ExprKind::Null => Expr::Null,
        ExprKind::Unit => Expr::Unit,
        ExprKind::Bool(val) => Expr::Bool(*val),
        ExprKind::Int(val, _orig_text) => Expr::Int(*val),
        ExprKind::Float(val) => Expr::Float(*val),
        ExprKind::String(val) => Expr::String(*val),
        ExprKind::Ident(name) => {
            if name.name.as_str() == "__SITE__" {
                Expr::String(ustr(&format!(
                    "{}:{}",
                    e.span.file,
                    e.span.line_range.0 + 1
                )))
            } else if name.name.as_str() == "_" && ctx.placeholder_param.is_some() {
                if ctx.placeholder_forbidden {
                    comp_error!(
                        e.span,
                        "`_` placeholder cannot appear inside an explicit lambda or block expression"
                    );
                }

                Expr::Var(ctx.placeholder_param.unwrap())
            } else {
                lookup_in_scope(ctx, body, scope, name)?
            }
        }

        ExprKind::InterpolatedString(parts) => {
            Expr::InterpolatedString(lower_interp_parts(ctx, a, body, scope, parts)?)
        }

        ExprKind::Block(block) => {
            let saved = ctx.placeholder_forbidden;

            ctx.placeholder_forbidden = true;
            let r = lower_block(ctx, a, body, scope, block);
            ctx.placeholder_forbidden = saved;

            return r;
        }

        ExprKind::FunctionCall {
            func,
            generic_args,
            args,
            syntax: _,
        } => {
            let generic_args = ctx.lower_type_exprs_to_range(
                a,
                body,
                TyPos::Body,
                generic_args,
                e.span,
            )?;
            match &a.e(func).kind {
            ExprKind::Ident(name) => {
                let args_range = lower_exprs(ctx, a, body, scope, args.iter().map(|x| &x.value))?;

                for (arg_idx, ca) in args_range.into_iter().zip(args.iter()) {
                    if let Some(nm) = &ca.name {
                        body.arg_names.insert(arg_idx, nm.name);
                    }
                    if ca.is_spread {
                        body.spread_args.insert(arg_idx);
                    }
                }

                // A local fn-typed binding is called through its value, everything else
                // resolves at a top-level by name. A v1 fragment resolves the
                // name through the namespace tiers to CallFamily::Resolved
                // (DefKey + hidden origins, spec 8.3).
                // A local is called through its value, and inference never asks
                // its family for a name, so a local resolves nothing further.
                let local = lookup_local(body, scope, name.name);
                let (callee_key, family) = if local.is_none() && name.name.as_str() != "_" {
                    ctx.resolve_callee_family(name.name, name.span)?
                } else if ctx.v1_scope().is_some() {
                    (name.name, hir::CallFamily::resolved(name.name, None, &[]))
                } else {
                    (name.name, hir::CallFamily::from_name(name.name))
                };
                let callee = match local {
                    Some(binding) => hir::Callee::Local(binding),
                    None if name.name.as_str() == "_" => match lookup_in_scope(ctx, body, scope, name)? {
                        Expr::Var(binding) => hir::Callee::Local(binding),
                        _ => hir::Callee::Unresolved { name: callee_key, name_span: name.span },
                    },
                    None => hir::Callee::Unresolved { name: callee_key, name_span: name.span },
                };

                Expr::Call {
                    callee,
                    args: args_range,
                    generic_args,
                    family,
                }
            }

            ExprKind::FieldAccess(receiver, method) => {
                if !generic_args.is_empty() {
                    comp_error!(e.span, "explicit type arguments are only supported on named function calls");
                }
                let all = std::iter::once(receiver).chain(args.iter().map(|a| &a.value));

                let args_range = lower_exprs(ctx, a, body, scope, all)?;

                for (arg_idx, ca) in args_range.into_iter().skip(1).zip(args.iter()) {
                    if let Some(nm) = &ca.name {
                        body.arg_names.insert(arg_idx, nm.name);
                    }
                    if ca.is_spread {
                        body.spread_args.insert(arg_idx);
                    }
                }

                body.method_call_recvs.insert(args_range.start());
                body.method_names.insert(args_range.start(), method.name);

                // UFCS and the bare call share one visible family: in a v1
                // fragment the method name goes through the same tier
                // resolution as `method(recv, ..)` would (spec 8.4).
                let (callee_key, family) = ctx.resolve_method_family(method.name, method.span)?;
                Expr::Call {
                    callee: Callee::Unresolved {
                        name: callee_key,
                        name_span: method.span,
                    },
                    args: args_range,
                    generic_args: IdxRange::new_empty(),
                    family,
                }
            }

            // `a::b::f(..)` in a v1 fragment. Without this the path lowers as a
            // VALUE (`Expr::Global`) and the call becomes `Callee::Value`, which
            // MIR cannot give a callee identity -- a direct call must carry the
            // family's DefKey exactly as the bare-name form does (plan 2.3).
            ExprKind::Path { path, generic_args: path_generics }
                if path_generics.is_empty() && ctx.v1_path_callable(path).is_some() =>
            {
                let (key, owner) = ctx.v1_path_callable(path).expect("guard resolved it");
                ctx.record_path(body, TyPos::Body, path, hir::SegmentTarget::Def(key));
                let args_range = lower_exprs(ctx, a, body, scope, args.iter().map(|x| &x.value))?;
                for (arg_idx, ca) in args_range.into_iter().zip(args.iter()) {
                    if let Some(nm) = &ca.name {
                        body.arg_names.insert(arg_idx, nm.name);
                    }
                }
                Expr::Call {
                    callee: hir::Callee::Unresolved {
                        name: key,
                        name_span: path.segments.last().map_or(e.span, |segment| segment.span),
                    },
                    args: args_range,
                    generic_args,
                    family: hir::CallFamily::resolved(key, owner, &[]),
                }
            }

            // `p::E::V(..)` in a v1 fragment: a TUPLE-payload variant
            // constructor, which the legacy grammar spells `E::V(..)` and lowers
            // through the `QualifiedIdent` arm below. Without this the callee
            // takes the value-position path route, where EVERY variant lowers
            // with `VariantArgs::Unit` -- right for `p::E::V`, but here it drops
            // the payload and inference rejects the construction as a
            // payload/argument mismatch.
            ExprKind::Path { path, generic_args: path_generics }
                if path_generics.is_empty() && ctx.v1_path_variant(path).is_some() =>
            {
                let (enum_key, variant) = ctx.v1_path_variant(path).expect("guard resolved it");
                ctx.record_path(body, TyPos::Body, path, hir::SegmentTarget::Variant { enum_key, variant });
                if !generic_args.is_empty() {
                    comp_error!(e.span, "explicit type arguments are not supported on enum constructors");
                }
                let args_range = lower_exprs(ctx, a, body, scope, args.iter().map(|x| &x.value))?;
                Expr::EnumVariant {
                    enum_name: enum_key,
                    enum_name_span: e.span,
                    variant,
                    variant_span: e.span,
                    args: VariantArgs::Tuple(args_range),
                }
            }

            ExprKind::QualifiedIdent { base, member }
                if ctx.variant_enum_key(base.name, base.span).is_ok_and(|key| key.is_some()) =>
            {
                if !generic_args.is_empty() {
                    comp_error!(e.span, "explicit type arguments are not supported on enum constructors");
                }
                let enum_key = ctx.variant_enum_key(base.name, base.span)?.expect("the guard found the enum");
                let args_range = lower_exprs(ctx, a, body, scope, args.iter().map(|x| &x.value))?;

                Expr::EnumVariant {
                    enum_name: enum_key,
                    enum_name_span: base.span,
                    variant: member.name,
                    variant_span: member.span,
                    args: VariantArgs::Tuple(args_range),
                }
            }

            ExprKind::SafeNavigation(receiver, method) => {
                let receiver = lower_expr(ctx, a, body, scope, a.e(receiver))?;
                let args_range = lower_exprs(ctx, a, body, scope, args.iter().map(|a| &a.value))?;

                for (arg_idx, ca) in args_range.into_iter().zip(args.iter()) {
                    if let Some(nm) = &ca.name {
                        body.arg_names.insert(arg_idx, nm.name);
                    }
                }

                body.method_names.insert(receiver, method.name);
                let (callee_key, family) = ctx.resolve_method_family(method.name, method.span)?;
                Expr::SafeNavCall {
                    receiver,
                    method: callee_key,
                    args: args_range,
                    family,
                }
            }

            _ => {
                if !generic_args.is_empty() {
                    comp_error!(e.span, "explicit type arguments require a named function callee");
                }
                let callee = lower_expr(ctx, a, body, scope, a.e(func))?;
                let args_range = lower_exprs(ctx, a, body, scope, args.iter().map(|a| &a.value))?;

                Expr::Call {
                    callee: hir::Callee::Value(callee),
                    args: args_range,
                    generic_args: IdxRange::new_empty(),
                    // The callee is a value, so inference calls its type and
                    // never asks the family for a name; the placeholder key is
                    // not a resolvable spelling.
                    family: hir::CallFamily::from_name(ustr("<value>")),
                }

                // comp_error!(
                //     func.span,
                //     "Only ident-based fn calls are supported, got {:?}",
                //     func.kind
                // );
            }
            }
        }

        ExprKind::ArrayLiteral(elems) => {
            let elems = lower_exprs(ctx, a, body, scope, elems.iter())?;
            Expr::Array(elems)
        }

        ExprKind::If {
            branches,
            else_branch,
        } => {
            let mut acc: Option<Idx<Expr>> = match else_branch {
                Some(blk) => {
                    let blk_expr = lower_block(ctx, a, body, scope, blk)?;
                    Some(ctx.alloc_block_expr(body, blk_expr, a.b(blk)))
                }

                None => None,
            };

            for (cond, block) in branches[1..].iter().rev() {
                let cond = lower_expr(ctx, a, body, scope, a.e(cond))?;
                let then_expr = lower_block(ctx, a, body, scope, block)?;
                let then = ctx.alloc_block_expr(body, then_expr, a.b(block));

                let idx = ctx.alloc_expr(
                    body,
                    Expr::If {
                        cond,
                        then_branch: then,
                        else_branch: acc,
                    },
                    a.b(block).span,
                );

                acc = Some(idx);
            }

            let (cond, block) = &branches[0];
            let cond = lower_expr(ctx, a, body, scope, a.e(cond))?;
            let block_expr = lower_block(ctx, a, body, scope, block)?;
            let then = ctx.alloc_block_expr(body, block_expr, a.b(block));

            Expr::If {
                cond,
                then_branch: then,
                else_branch: acc,
            }
        }

        ExprKind::Tuple(items) => Expr::Tuple(lower_exprs(ctx, a, body, scope, items.iter())?),

        ExprKind::BinOp {
            left, op, right, ..
        } if *op == skull_parser::TokenKind::DoubleDot => Expr::Range {
            low: lower_expr(ctx, a, body, scope, a.e(left))?,
            high: lower_expr(ctx, a, body, scope, a.e(right))?,
        },

        ExprKind::BinOp {
            left, op, right, ..
        } if matches!(
            op,
            TokenKind::AddAssign
                | TokenKind::SubAssign
                | TokenKind::MulAssign
                | TokenKind::DivAssign
        ) =>
        {
            let op_fn = match op {
                TokenKind::AddAssign => "__op_add",
                TokenKind::SubAssign => "__op_sub",
                TokenKind::MulAssign => "__op_mul",
                TokenKind::DivAssign => "__op_div",

                _ => unreachable!(),
            };

            let target = lower_expr(ctx, a, body, scope, a.e(left))?;
            // The same callee resolution as the binary operator call, so
            // `a += b` sees what `a + b` sees (spec 3.2, 8.5).
            let (callee_key, family) = ctx.resolve_callee_family(ustr(op_fn), e.span)?;

            if lvalue_has_safe_nav(a, left) {
                let value = lower_expr(ctx, a, body, scope, a.e(right))?;
                Expr::CompoundAssign {
                    target,
                    op_fn: ustr(op_fn),
                    value,
                    family,
                }
            } else {
                let args = lower_exprs(ctx, a, body, scope, [left, right].into_iter())?;

                let call = ctx.alloc_expr(
                    body,
                    Expr::Call {
                        callee: Callee::Unresolved {
                            name: callee_key,
                            name_span: e.span,
                        },
                        args,
                        generic_args: IdxRange::new_empty(),
                        family,
                    },
                    e.span,
                );

                Expr::Assign {
                    target,
                    value: call,
                }
            }
        }

        ExprKind::BinOp {
            left, op, right, ..
        } => {
            use skull_parser::TokenKind;

            let bin_op = match op {
                TokenKind::DoubleQuestionMark => hir::BinOp::NullCoalesce,
                TokenKind::Equals => hir::BinOp::Eq,
                TokenKind::NotEquals => hir::BinOp::NotEq,
                TokenKind::LessThan => hir::BinOp::Lt,
                TokenKind::LessThanEqual => hir::BinOp::Leq,
                TokenKind::GreaterThan => hir::BinOp::Gt,
                TokenKind::GreaterThanEqual => hir::BinOp::Geq,
                TokenKind::And => hir::BinOp::And,
                TokenKind::Or => hir::BinOp::Or,

                TokenKind::BitAnd => hir::BinOp::BitAnd,
                TokenKind::BitOr => hir::BinOp::BitOr,
                TokenKind::BitXor => hir::BinOp::BitXor,
                TokenKind::LeftShift => hir::BinOp::Shl,
                TokenKind::RightShift => hir::BinOp::Shr,
                _ => comp_error!(e.span, "Unsupported binary operator {:?}", op),
            };

            Expr::BinOp {
                op: bin_op,
                lhs: lower_expr(ctx, a, body, scope, a.e(left))?,
                rhs: lower_expr(ctx, a, body, scope, a.e(right))?,
            }
        }

        ExprKind::StructLiteral { name, bases, fields } => {
            // Functional-update bases evaluate before fields.
            let base_range = lower_exprs(ctx, a, body, scope, bases.iter())?;

            let mut entries = Vec::new();

            for (names, value) in fields.iter() {
                let value_idx = lower_expr(ctx, a, body, scope, a.e(value))?;

                for field_name in names {
                    entries.push(hir::StructLitField {
                        name: field_name.name,
                        name_span: field_name.span,
                        value: value_idx,
                    });
                }
            }

            let fields = alloc_contiguous(&mut body.struct_lit_field_arena, entries);

            Expr::StructLiteral {
                name: ctx.resolve_bare_key(name.name, name.span)?,
                fields,
                bases: base_range,
            }
        }

        ExprKind::FieldAccess(expr, field) => match &a.e(expr).kind {
            ExprKind::Ident(base) if base.name.as_str() == "_G" => Expr::Global(field.name),
            _ => {
                let base = lower_expr(ctx, a, body, scope, a.e(expr))?;

                if let Some(index) = field.tuple_index() {
                    Expr::TupleIndex { tuple: base, index }
                } else {
                    Expr::FieldAccess {
                        base,
                        field: field.name,
                    }
                }
            }
        },

        // Modules-v1 value-position paths: `p::f` (callable family/global),
        // `p::CONST`, `p::E::V`, or `p::Type` used as a value.
        ExprKind::Path { path, generic_args } => {
            if !generic_args.is_empty() {
                return Err(CompilerError::new_with_label(
                    e.span,
                    "generic arguments on a qualified path are not valid in value position"
                        .to_string(),
                ));
            }
            let Some(frag) = ctx.v1_scope() else {
                return Err(CompilerError::new_with_label(
                    e.span,
                    "qualified paths are modules-v1 grammar; this file is legacy".to_string(),
                ));
            };
            let use_path = bone_core::modules::UsePathRef {
                from_root: path.from_root,
                segments: path.segments.iter().map(|i| i.name).collect(),
            };
            let target = ctx.probed(|| resolve::resolve_member_path(ctx.workspace, frag, &use_path));
            if let Ok(t) = &target {
                ctx.record_path(body, TyPos::Body, path, member_segment_target(t));
            }
            match target {
                Ok(resolve::MemberTarget::Global(key) | resolve::MemberTarget::Callable { key, .. }) => {
                    Expr::Global(key)
                }
                Ok(resolve::MemberTarget::EnumVariant { enum_key, variant }) => {
                    Expr::EnumVariant {
                        enum_name: enum_key,
                        enum_name_span: e.span,
                        variant,
                        variant_span: e.span,
                        args: hir::VariantArgs::Unit,
                    }
                }
                Ok(resolve::MemberTarget::Type(key)) => Expr::Global(key),
                Err(msg) => return Err(CompilerError::new_with_label(e.span, msg)),
            }
        }

        // `p::Type { fields }` / `p::E::V { fields }` — a qualified literal.
        ExprKind::PathStructLiteral { path, bases, fields } => {
            let Some(frag) = ctx.v1_scope() else {
                return Err(CompilerError::new_with_label(
                    e.span,
                    "qualified record literals are modules-v1 grammar; this file is legacy".to_string(),
                ));
            };
            let use_path = bone_core::modules::UsePathRef {
                from_root: path.from_root,
                segments: path.segments.iter().map(|i| i.name).collect(),
            };
            let target = match ctx.probed(|| resolve::resolve_member_path(ctx.workspace, frag, &use_path)) {
                Ok(t) => t,
                Err(msg) => return Err(CompilerError::new_with_label(e.span, msg)),
            };
            ctx.record_path(body, TyPos::Body, path, member_segment_target(&target));
            if !bases.is_empty() {
                return Err(CompilerError::new_with_label(
                    e.span,
                    "functional-update `..base` is not supported on qualified record literals"
                        .to_string(),
                ));
            }
            let mut entries = Vec::new();
            for (names, value) in fields.iter() {
                let v = lower_expr(ctx, a, body, scope, a.e(value))?;
                for fname in names {
                    entries.push(StructLitField {
                        name: fname.name,
                        name_span: fname.span,
                        value: v,
                    });
                }
            }
            let fields_range = alloc_contiguous(&mut body.struct_lit_field_arena, entries);
            match target {
                resolve::MemberTarget::Type(key) => Expr::StructLiteral {
                    name: key,
                    fields: fields_range,
                    bases: lower_exprs(ctx, a, body, scope, std::iter::empty())?,
                },
                resolve::MemberTarget::EnumVariant { enum_key, variant } => {
                    Expr::EnumVariant {
                        enum_name: enum_key,
                        enum_name_span: e.span,
                        variant,
                        variant_span: e.span,
                        args: hir::VariantArgs::Record(fields_range),
                    }
                }
                resolve::MemberTarget::Global(key) | resolve::MemberTarget::Callable { key, .. } => {
                    return Err(CompilerError::new_with_label(
                        e.span,
                        format!("`{key}` names a value, not a record"),
                    ));
                }
            }
        }

        ExprKind::SiblingFieldRef(ident) => Expr::SiblingRef(ident.name),

        ExprKind::QualifiedIdent { base, member } => {
            if let Some(enum_key) = ctx.variant_enum_key(base.name, base.span)? {
                Expr::EnumVariant {
                    enum_name: enum_key,
                    enum_name_span: base.span,
                    variant: member.name,
                    variant_span: member.span,
                    args: hir::VariantArgs::Unit,
                }
            } else {
                comp_error!(
                    base.span,
                    "Unknown qualified identifier base `{}`",
                    base.name
                )
            }
        }

        ExprKind::EnumVariantRecord {
            enum_name,
            variant_name,
            fields,
        } => {
            let mut entries = Vec::new();

            for (names, value) in fields.iter() {
                let v = lower_expr(ctx, a, body, scope, a.e(value))?;

                for fname in names {
                    entries.push(StructLitField {
                        name: fname.name,
                        name_span: fname.span,
                        value: v,
                    });
                }
            }

            Expr::EnumVariant {
                enum_name: ctx.variant_enum_key(enum_name.name, enum_name.span)?.unwrap_or(enum_name.name),
                enum_name_span: enum_name.span,
                variant: variant_name.name,
                variant_span: variant_name.span,

                args: hir::VariantArgs::Record(alloc_contiguous(
                    &mut body.struct_lit_field_arena,
                    entries,
                )),
            }
        }

        ExprKind::Assign {
            target,
            value,
            compound_op,
            ..
        } => {
            let target = lower_expr(ctx, a, body, scope, a.e(target))?;
            let value = lower_expr(ctx, a, body, scope, a.e(value))?;

            if let Some(op_tok) = compound_op {
                let op_fn = op_tok.overloadable_binop_fn().ok_or_else(|| {
                    CompilerError::new_with_label(
                        e.span,
                        format!("Unsupported compound assignment operator `{}`", op_tok),
                    )
                })?;

                let (_, family) = ctx.resolve_callee_family(ustr(op_fn), e.span)?;
                Expr::CompoundAssign {
                    target,
                    op_fn: ustr(op_fn),
                    value,
                    family,
                }
            } else {
                Expr::Assign { target, value }
            }
        }

        ExprKind::Match { scrutinee, arms } => {
            let scrutinee = lower_expr(ctx, a, body, scope, a.e(scrutinee))?;
            let mut arm_vals = Vec::with_capacity(arms.len());

            for arm in arms.iter() {
                let arm_scope = body.alloc_child_scope(scope);

                let pat = lower_pattern_value(ctx, a, body, arm_scope, &arm.pattern)?;
                let pattern = body.pattern_arena.alloc(pat);

                let body_block = lower_block(ctx, a, body, arm_scope, &arm.body)?;
                let body_i = ctx.alloc_block_expr(body, body_block, a.b(&arm.body));

                let guard = match &arm.guard {
                    Some(g) => Some(lower_expr(ctx, a, body, arm_scope, a.e(g))?),
                    None => None,
                };

                arm_vals.push(MatchArm {
                    pattern,
                    body: body_i,
                    guard,
                });
            }

            let arms = alloc_contiguous(&mut body.match_arm_arena, arm_vals);
            Expr::Match { scrutinee, arms }
        }

        ExprKind::Paren(inner) => lower_expr_value(ctx, a, body, scope, a.e(inner))?,
        ExprKind::Not(inner) => {
            let family = ctx.resolve_unary_family(ustr("__not"), e.span)?;
            Expr::Not(lower_expr(ctx, a, body, scope, a.e(inner))?, family)
        }
        ExprKind::BitNot(inner) => Expr::BitNot(lower_expr(ctx, a, body, scope, a.e(inner))?),

        ExprKind::As(inner, ty) => Expr::As {
            value: lower_expr(ctx, a, body, scope, a.e(inner))?,
            target: lower_type_expr_local(ctx, a, body, a.te(ty))?,
            kind: AsKind::Cast,
        },

        ExprKind::Bitcast(inner, ty) => Expr::As {
            value: lower_expr(ctx, a, body, scope, a.e(inner))?,
            target: lower_type_expr_local(ctx, a, body, a.te(ty))?,
            kind: AsKind::Bitcast,
        },

        ExprKind::Index(base, index) => Expr::Index {
            base: lower_expr(ctx, a, body, scope, a.e(base))?,
            index: lower_expr(ctx, a, body, scope, a.e(index))?,
        },

        ExprKind::SafeNavigation(base, field) => Expr::SafeNav {
            base: lower_expr(ctx, a, body, scope, a.e(base))?,
            field: field.name,
        },

        ExprKind::Lambda {
            params,
            body: body_e,
            ..
        } => {
            let lambda_scope = body.alloc_child_scope(scope);

            let mut binding_idxs = Vec::with_capacity(params.len());

            for p in params {
                let declared_ty = match &p.ty {
                    Some(te) => Some(lower_type_expr_local(ctx, a, body, a.te(te))?),
                    None => None,
                };

                let b = ctx.alloc_binding(
                    body,
                    BindingInfo {
                        name: p.name.name,
                        kind: hir::BindingKind::Param,
                        declared_ty,
                        span: p.name.span,
                    },
                    p.span,
                );

                body.scope_arena[lambda_scope]
                    .bindings
                    .insert(p.name.name, b);

                binding_idxs.push(b);
            }

            let params_range = match (binding_idxs.first(), binding_idxs.last()) {
                (Some(a), Some(b)) => IdxRange::new_inclusive(*a..=*b),
                _ => IdxRange::new_empty(),
            };

            let saved = ctx.placeholder_forbidden;
            ctx.placeholder_forbidden = true;
            let body_expr = lower_block(ctx, a, body, lambda_scope, body_e)?;
            ctx.placeholder_forbidden = saved;

            let body_idx = ctx.alloc_block_expr(body, body_expr, a.b(body_e));

            Expr::Lambda {
                params: params_range,
                body: body_idx,
            }
        }

        ExprKind::UnaryMinus(inner) => {
            let family = ctx.resolve_unary_family(ustr("__unm"), e.span)?;
            Expr::UnaryMinus(lower_expr(ctx, a, body, scope, a.e(inner))?, family)
        }

        ExprKind::Yield(opt) => {
            let inner = match opt {
                Some(e) => Some(lower_expr(ctx, a, body, scope, a.e(e))?),
                None => None,
            };

            Expr::Yield(inner)
        }

        ExprKind::YieldDelegate(inner) => {
            Expr::YieldDelegate(lower_expr(ctx, a, body, scope, a.e(inner))?)
        }

        ExprKind::IfLet {
            pattern,
            value,
            then_block,
            else_block,
        } => {
            let clauses = [IfLetClause::Binding {
                pattern: pattern.clone(),
                value: *value,
            }];
            let idx = lower_if_let_clauses(ctx, a, body, scope, &clauses, then_block, else_block)?;
            body.expr_arena[idx]
        }

        ExprKind::IfLetChain {
            clauses,
            then_block,
            else_block,
        } => {
            let idx = lower_if_let_clauses(ctx, a, body, scope, clauses, then_block, else_block)?;
            body.expr_arena[idx]
        }

        ExprKind::Spread(inner) => Expr::Spread(lower_expr(ctx, a, body, scope, a.e(inner))?),

        ExprKind::SafeNavigationLet {
            receiver,
            binding,
            body: block,
        } => {
            let receiver = lower_expr(ctx, a, body, scope, a.e(receiver))?;

            let body_scope = body.alloc_child_scope(scope);

            let b = ctx.alloc_binding(
                body,
                BindingInfo {
                    name: binding.name,
                    kind: BindingKind::Let,
                    declared_ty: None,
                    span: binding.span,
                },
                binding.span,
            );

            body.scope_arena[body_scope]
                .bindings
                .insert(binding.name, b);

            let body_expr = lower_block(ctx, a, body, body_scope, block)?;
            let body = ctx.alloc_block_expr(body, body_expr, a.b(block));

            Expr::SafeNavLet {
                receiver,
                binding: b,
                body,
            }
        }

        ExprKind::TypeAscription(inner, ty) | ExprKind::ParenAscription(inner, ty) => Expr::As {
            value: lower_expr(ctx, a, body, scope, a.e(inner))?,
            target: lower_type_expr_local(ctx, a, body, a.te(ty))?,
            kind: AsKind::Ascribe,
        },

        ExprKind::PreIncrement(inner) => Expr::IncDec {
            target: lower_expr(ctx, a, body, scope, a.e(inner))?,
            decrement: false,
            postfix: false,
        },

        ExprKind::PostIncrement(inner) => Expr::IncDec {
            target: lower_expr(ctx, a, body, scope, a.e(inner))?,
            decrement: false,
            postfix: true,
        },

        ExprKind::PreDecrement(inner) => Expr::IncDec {
            target: lower_expr(ctx, a, body, scope, a.e(inner))?,
            decrement: true,
            postfix: false,
        },

        ExprKind::PostDecrement(inner) => Expr::IncDec {
            target: lower_expr(ctx, a, body, scope, a.e(inner))?,
            decrement: true,
            postfix: true,
        },

        ExprKind::ReturnExpr(inner) => Expr::Return(match inner {
            Some(e) => Some(lower_expr(ctx, a, body, scope, a.e(e))?),
            None => None,
        }),

        ExprKind::BreakExpr => Expr::Break,
        ExprKind::ContinueExpr => Expr::Continue,

        ExprKind::FunctionRef(ident) => {
            let key = ctx.resolve_bare_key(ident.name, ident.span)?;
            Expr::Global(key)
        }

        ExprKind::AnaphoricIdent(name) => lookup_in_scope(ctx, body, scope, name)?,

        ExprKind::Hole(_, name) => Expr::Hole(*name),

        ExprKind::QuestionMark(..) => {
            comp_error!(e.span, "unsupported expr {:?}", e.kind)
        }
    };

    Ok(result)
}

pub fn lower_expr(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    scope: Idx<Scope>,
    e: &skull_parser::ExprData,
) -> Result<Idx<hir::Expr>, CompilerError> {
    if let ExprKind::Block(b) = &e.kind {
        let saved = ctx.placeholder_forbidden;
        ctx.placeholder_forbidden = true;
        let r = lower_block(ctx, a, body, scope, b);
        ctx.placeholder_forbidden = saved;

        return Ok(ctx.alloc_block_expr(body, r?, a.b(b)));
    }

    let hir_e = lower_expr_value(ctx, a, body, scope, e)?;
    Ok(alloc_value(ctx, a, body, e, hir_e))
}

/// Allocate `hir_e`, lowered from `e`. Every site that allocates a lowered
/// expression next to its source goes through here, so a v1 function value
/// records the view that owns it wherever it appears.
fn alloc_value(ctx: &mut LowerCtx, a: &Ast, body: &mut Body, e: &skull_parser::ExprData, hir_e: Expr) -> Idx<Expr> {
    let idx = ctx.alloc_expr(body, hir_e, e.span);
    if let Expr::Global(key) = hir_e
        && let Some(owner) = value_owner(ctx, a, e, key)
    {
        body.value_owners.insert(idx, owner);
    }
    idx
}

/// The view owning a legacy callable a v1 value expression names
/// (`Body::value_owners`): the lookup that lowered `e` to
/// `Expr::Global(key)`, asked again for its owner. Only a definition with no
/// `DefMeta` can have one, so a v1 definition is not looked up twice.
fn value_owner(ctx: &LowerCtx, a: &Ast, e: &skull_parser::ExprData, key: Ustr) -> Option<bone_core::modules::PackageId> {
    let frag = ctx.v1_scope()?;
    if ctx.workspace.defs.contains_key(&key) {
        return None;
    }
    match &e.kind {
        ExprKind::Paren(inner) => value_owner(ctx, a, a.e(inner), key),
        ExprKind::Ident(name) | ExprKind::FunctionRef(name) | ExprKind::AnaphoricIdent(name) => {
            ctx.probed(|| resolve::resolve_bare(ctx.workspace, Some(frag), name.name)).ok()?.owner
        }
        ExprKind::Path { path, .. } => {
            let use_path = bone_core::modules::UsePathRef {
                from_root: path.from_root,
                segments: path.segments.iter().map(|i| i.name).collect(),
            };
            match ctx.probed(|| resolve::resolve_member_path(ctx.workspace, frag, &use_path)) {
                Ok(resolve::MemberTarget::Callable { owner, .. }) => owner,
                _ => None,
            }
        }
        _ => None,
    }
}

fn function_footprint_lo(annotations: &[Annotation], f: &FunctionDef) -> usize {
    annotations
        .iter()
        .map(|a| a.span.offset_range.0)
        .min()
        .unwrap_or(f.span.offset_range.0)
}

/// Lower a fn's generic bounds: `I: Iterator<T>` becomes `Some(elem)` at I's
/// index. Runs AFTER `declare_generics` so a bound may reference any sibling
/// generic, including a later one (`<I: Iterator<T>, T>`). The only bound the
/// compiler knows is the builtin `Iterator<elem>`; anything else rejects.
fn lower_generic_bounds(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    generics: &[skull_parser::Generic],
) -> Result<Vec<Option<TyId>>, CompilerError> {
    let mut out = Vec::with_capacity(generics.len());
    for g in generics {
        let Some(b) = g.bound else {
            out.push(None);
            continue;
        };
        let bte = a.te(&b);
        match &bte.kind {
            skull_parser::TypeExprKind::App { ctor, args }
                if bone_core::modules::is_intrinsic_key(
                    ctx.resolve_bare_key(ctor.name, ctor.span)?,
                    "Iterator",
                ) && args.len() == 1 =>
            {
                out.push(Some(lower_type_expr(ctx, a, body, a.te(&args[0]))?));
            }
            skull_parser::TypeExprKind::App { ctor, .. }
                if bone_core::modules::is_intrinsic_key(
                    ctx.resolve_bare_key(ctor.name, ctor.span)?,
                    "Iterator",
                ) =>
            {
                comp_error!(
                    bte.span,
                    "`Iterator` bound takes exactly one element type: `{}: Iterator<T>`",
                    g.name.name
                );
            }
            _ => {
                comp_error!(
                    bte.span,
                    "unknown bound on `{}`: the only supported bound is `Iterator<T>`",
                    g.name.name
                );
            }
        }
    }
    Ok(out)
}

/// Generic bounds only mean something on FNS (structural `next` conformance at
/// call sites); on a type declaration there is no call site to check them at.
fn reject_generic_bounds(
    what: &str,
    generics: &[skull_parser::Generic],
) -> Result<(), CompilerError> {
    for g in generics {
        if g.bound.is_some() {
            comp_error!(
                g.name.span,
                "generic bounds are not allowed on {}; bounds only apply to fn generic parameters",
                what
            );
        }
    }
    Ok(())
}

/// The trailing generic defaults recorded for `name` (empty when none).
/// Borrowed from the workspace ('w), so callers can keep the slice across
/// later `&mut LowerCtx` uses.
fn generic_defaults(ws: &Workspace, name: Ustr) -> &[hir::GenericDefault] {
    ws.type_generic_defaults.get(&name).map(Vec::as_slice).unwrap_or(&[])
}

/// Synthesize the `TypeExpr` a default pads in at a use site: exactly what the
/// source form would have lowered to (a builtin, or a bare nominal `App`).
fn default_type_expr(d: hir::GenericDefault) -> TypeExpr {
    match d {
        hir::GenericDefault::Builtin(b) => TypeExpr::Builtin(b),
        hir::GenericDefault::Named(n) => TypeExpr::App {
            ctor: hir::TypeCtor::for_name(n),
            params: IdxRange::new_empty(),
        },
    }
}

/// Defaults are supported on type-decl generics only; call-site defaulting for
/// fn generics needs inference work, so a fn default rejects (fail closed, not
/// silently ignored).
fn reject_generic_defaults(
    what: &str,
    generics: &[skull_parser::Generic],
) -> Result<(), CompilerError> {
    for g in generics {
        if g.default.is_some() {
            comp_error!(
                g.name.span,
                "default type params are not supported on {}; only struct/enum/newtype generics may carry defaults",
                what
            );
        }
    }
    Ok(())
}

/// Name-RESOLUTION check for a decl's generic defaults: lower each default
/// normally (result discarded) so an unknown type name or a bare generic type
/// (`= Entity` for `Entity<T>`) errors at the DECL's span rather than at some
/// distant use site's pad. Runs after `validate_generic_defaults`, so sibling
/// refs never reach the generics-in-scope lookup.
fn lower_generic_defaults_for_check(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    generics: &[skull_parser::Generic],
) -> Result<(), CompilerError> {
    for g in generics {
        if let Some(d) = &g.default {
            lower_type_expr(ctx, a, body, a.te(d))?;
        }
    }
    Ok(())
}

/// Decl-side validation for type-decl generic defaults (the authoritative,
/// spanned counterpart of the pre-pass classification): defaults must be
/// trailing, index-free (a builtin or a bare named type), and must not
/// reference a sibling type parameter. Name RESOLUTION of a `Named` default is
/// checked by the caller lowering the default expr (unknown / bare-generic
/// names error there with the decl's span).
fn validate_generic_defaults(
    what: &str,
    generics: &[skull_parser::Generic],
    a: &Ast,
) -> Result<(), CompilerError> {
    let mut first_defaulted: Option<Ustr> = None;
    for g in generics {
        let Some(d) = &g.default else {
            if let Some(prev) = first_defaulted {
                comp_error!(
                    g.name.span,
                    "type parameter `{}` without a default follows defaulted `{}`; defaults must be trailing",
                    g.name.name,
                    prev
                );
            }
            continue;
        };
        first_defaulted.get_or_insert(g.name.name);

        let te = a.te(d);
        use skull_parser::TypeExprKind as K;
        let ok = !te.is_nullable
            && match &te.kind {
                K::Int | K::Float | K::Bool | K::String | K::Unit | K::Any => true,
                K::Ident(n) => {
                    if generics.iter().any(|s| s.name.name == n.name) {
                        comp_error!(
                            te.span,
                            "default for type parameter `{}` may not reference sibling type parameter `{}`",
                            g.name.name,
                            n.name
                        );
                    }
                    n.name.as_str() != "_"
                }
                _ => false,
            };
        if !ok {
            comp_error!(
                te.span,
                "unsupported default for type parameter `{}` on {}: only builtin types and bare type names are supported",
                g.name.name,
                what
            );
        }
    }
    Ok(())
}

pub fn build_function(
    ctx: &mut LowerCtx,
    a: &Ast,
    f: &FunctionDef,
    annotations: &[Annotation],
) -> Result<(Function, Option<Body>, Vec<CompilerError>), CompilerError> {
    ctx.generics_in_scope.clear();
    reject_generic_defaults("fn generics", &f.generics)?;
    let generic_var_ids = ctx.declare_generics(&f.generics);
    let mut body = Body::default();
    let generic_bounds = lower_generic_bounds(ctx, a, &mut body, &f.generics)?;

    let return_ty = if let Some(ret_ty) = &f.return_ty {
        let typ_expr = lower_type_expr(ctx, a, &mut body, a.te(ret_ty))?;
        Some(typ_expr)
    } else {
        None
    };

    let mut param_decls = Vec::with_capacity(f.params.len());
    let mut param_tys = Vec::with_capacity(f.params.len());

    for p in f.params.iter() {
        let ty_ref = lower_type_expr(ctx, a, &mut body, a.te(&p.ty))?;
        param_tys.push(ty_ref);
        param_decls.push(ParamDecl {
            name: p.name.name,
            ty: ty_ref,
            has_default: p.default.is_some(),
            annotations: lower_annotations_with_ctx(ctx, a, &p.annotations),
        });
    }

    // Registry violations accumulate as lowering errors (the recovery channel)
    // rather than failing fast: the workspace must stay buildable while an
    // annotation is mid-typed (decorator completion runs on it), and
    // `body_errors()` already turns any lowering error into a hard verdict.
    let annotations = lower_annotations_with_ctx(ctx, a, annotations);
    let mut anno_errors = Vec::new();
    if let Err(e) = validate_annotations(AnnoPos::FnItem, &annotations, &ctx.workspace.decorator_names) {
        anno_errors.push(e);
    }
    for pd in param_decls.iter() {
        if let Err(e) = validate_annotations(AnnoPos::Param, &pd.annotations, &ctx.workspace.decorator_names) {
            anno_errors.push(e);
        }
    }

    let (body_failed, inline_body, is_extern, ffi_lib, backend, mut errors) =
        lower_fn_body_into(ctx, a, f, &mut body, &param_tys)?;
    errors.extend(anno_errors);
    // Needs the body kind, which only exists after the lower above.
    if let Err(e) = bone_core::annotations::validate_param_repr_against_body(
        f.name.name,
        &param_decls,
        inline_body.is_some() || is_extern,
    ) {
        errors.push(e);
    }

    let is_variadic = f.params.last().map_or(false, |p| p.is_variadic);
    let num_required = f
        .params
        .iter()
        .filter(|p| p.default.is_none() && !p.is_variadic)
        .count();

    let func = hir::Function {
        span: f.span,
        name: f.name.name,
        name_span: f.name.span,

        is_coroutine: f.is_coroutine,
        is_variadic,
        generics: generic_var_ids,
        generic_bounds,
        param_decls,

        num_required,
        return_ty,
        inline_body,
        is_extern,
        backend,
        annotations,
        ffi_lib,
        is_synthetic: f.is_synthetic,
    };

    Ok((func, if body_failed { None } else { Some(body) }, errors))
}

/// Lower a function's BODY into `body`, given its already-resolved param types
/// (`param_tys`, one per `f.params`, in order). This allocates ONLY into `body`
/// (its binding / expr / scope / `lowered_tys` arenas) -- it never touches the
/// module `ty_arena`. That invariant is what lets the body-edit fast path
/// (`relower_function_in_place`) reuse the unchanged signature's interface
/// types instead of re-lowering them: re-lowering a signature appends fresh
/// entries to `module.ty_arena` and orphans the old ones, leaking one slot per
/// edit (pinned by the `bone-incremental` `sentinel_body_edit_leaks_ty_arena_entries`).
///
/// Returns the body-kind flags (`body_failed`, `inline_body`, `is_extern`,
/// `ffi_lib`, `backend`); the caller owns assembling / keeping the `Function`
/// signature.
fn lower_fn_body_into(
    ctx: &mut LowerCtx,
    a: &Ast,
    f: &FunctionDef,
    body: &mut Body,
    param_tys: &[TyId],
) -> Result<
    (
        bool,
        Option<(hir::FnBackend, Ustr)>,
        bool,
        Option<Ustr>,
        hir::FnBackend,
        Vec<CompilerError>,
    ),
    CompilerError,
> {
    debug_assert_eq!(
        f.params.len(),
        param_tys.len(),
        "lower_fn_body_into needs one resolved param ty per AST param"
    );

    // Collected (not pushed to the module) so the caller can key them by body --
    // a body re-lower must REPLACE its errors, not append. See `Module::lowering_errors`.
    let mut errors = Vec::new();
    let mut bindings = FxHashMap::default();
    let mut params = Vec::with_capacity(f.params.len());
    let mut param_defaults = Vec::with_capacity(f.params.len());

    let default_scope = body.alloc_root_scope();

    for (p, &ty_ref) in f.params.iter().zip(param_tys) {
        let b_idx = ctx.alloc_binding(
            body,
            BindingInfo {
                name: p.name.name,
                kind: hir::BindingKind::Param,
                declared_ty: Some(ty_ref),
                span: p.name.span,
            },
            p.span,
        );

        bindings.insert(p.name.name, b_idx);
        params.push(b_idx);

        let param_default = match &p.default {
            Some(expr) => Some(lower_expr(ctx, a, body, default_scope, a.e(expr))?),
            None => None,
        };
        param_defaults.push(param_default);
    }

    let range = match (params.first(), params.last()) {
        (Some(first), Some(last)) => IdxRange::new_inclusive(*first..=*last),
        _ => IdxRange::new_empty(),
    };

    let root_scope = body.alloc_root_scope_with_bindings(bindings);
    let mut body_failed = false;

    // `(inline_body, is_extern, ffi_lib, backend)`. `backend` is the codegen
    // discriminator: a Rock body is `Any` (portable); `extern`/`extern(lua)`/ffi
    // are luajit-native (`Lua`); `extern(js)` is `Js`; an inline body carries its
    // own backend.
    let (body_e, inline_body, is_extern, ffi_lib, backend) = match &f.body {
        skull_parser::FunctionBody::Rock(block) => {
            let b = a.b(block);

            match lower_block(ctx, a, body, root_scope, block) {
                Ok(lowered) => (
                    Some(ctx.alloc_block_expr(body, lowered, b)),
                    None,
                    false,
                    None,
                    hir::FnBackend::Any,
                ),

                Err(e) => {
                    errors.push(e);
                    body_failed = true;

                    (None, None, false, None, hir::FnBackend::Any)
                }
            }
        }

        skull_parser::FunctionBody::Inline(be, src) => {
            (None, Some((*be, Ustr::from(src.as_str()))), false, None, *be)
        }

        skull_parser::FunctionBody::ExternC | skull_parser::FunctionBody::ExternLua => {
            (None, None, true, None, hir::FnBackend::Lua)
        }

        skull_parser::FunctionBody::ExternJs => (None, None, true, None, hir::FnBackend::Js),

        skull_parser::FunctionBody::Ffi(lib) => (
            None,
            None,
            true,
            Some(Ustr::from(lib.as_str())),
            hir::FnBackend::Lua,
        ),
    };

    body.root = body_e;
    body.root_scope = Some(root_scope);
    body.param_bindings = range;
    body.param_defaults = param_defaults;

    Ok((body_failed, inline_body, is_extern, ffi_lib, backend, errors))
}

pub fn lower_function_def(
    ctx: &mut LowerCtx,
    a: &Ast,
    f: &FunctionDef,
    annotations: &[Annotation],
    item_lo: usize,
    reg_key: Ustr,
) -> Result<Idx<Function>, CompilerError> {
    let (mut func, body, errors) = build_function(ctx, a, f, annotations)?;
    func.name = reg_key;
    let idx = ctx.module.functions.alloc(func);
    let owner = BodyOwner::Function(idx);

    if !errors.is_empty() {
        ctx.module.lowering_errors.insert(owner, errors);
    }

    if let Some(body) = body {
        ctx.module.insert_body(owner, body);

        ctx.module.make_lapper(
            item_lo.min(function_footprint_lo(annotations, f)),
            f.span.offset_range.1,
            owner,
        );
    }

    Ok(idx)
}

pub fn relower_function_in_place(
    ws: &Workspace,
    module: &mut hir::Module,
    a: &Ast,
    f: &FunctionDef,
    annotations: &[Annotation],
    fid: Idx<Function>,
) -> Result<bool, CompilerError> {
    let owner = BodyOwner::Function(fid);
    ws.relowered_body_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Body-only fast path. The caller (`classify`) guarantees the signature TEXT
    // is unchanged, so REUSE the existing signature -- crucially its module
    // `ty_arena` indices -- and re-lower only the body. Re-running the full
    // `build_function` would re-lower the signature, appending fresh interface
    // type-exprs to `module.ty_arena` and orphaning the old ones: one leaked slot
    // per edit, unbounded over an editing session (pinned by the `bone-incremental`
    // `sentinel_body_edit_leaks_ty_arena_entries`).
    let sig_reusable = module.functions[fid].param_decls.len() == f.params.len()
        && module.functions[fid].generics.len() == f.generics.len();

    if !sig_reusable {
        // Defensive: the signature actually changed (classify should have tiered
        // this as an Interface edit). Fall back to a full rebuild of the fn --
        // correctness over the no-leak optimization.
        let high_water = module.lower_var_high_water;
        let mut ctx = LowerCtx {
            workspace: ws,
            module,
            generics_in_scope: Default::default(),
            _next_var_id: high_water,
            placeholder_param: None,
            placeholder_forbidden: false,
            scope_probes: Default::default(),
        };

        let (func, body, errors) = build_function(&mut ctx, a, f, annotations)?;
        ctx.flush_scope_probes();

        ctx.module.lower_var_high_water = ctx._next_var_id;
        ctx.module.functions[fid] = func;
        // REPLACE this body's lowering errors (clears stale ones from a prior edit).
        if errors.is_empty() {
            ctx.module.lowering_errors.remove(&owner);
        } else {
            ctx.module.lowering_errors.insert(owner, errors);
        }

        return Ok(match body {
            Some(body) => {
                ctx.module.insert_body(owner, body);
                true
            }
            None => {
                ctx.module.bodies.remove(&owner);
                false
            }
        });
    }

    // Reuse the existing signature's resolved param types + generic `TyId`s
    // (snapshot before the `&mut` borrow). `lower_fn_body_into` allocates only
    // into the fresh `Body`, never the module arena.
    let param_tys: Vec<TyId> = module.functions[fid]
        .param_decls
        .iter()
        .map(|p| p.ty)
        .collect();
    let generics_in_scope: FxHashMap<Ustr, TyId> = f
        .generics
        .iter()
        .map(|g| g.name.name)
        .zip(module.functions[fid].generics.iter().copied())
        .collect();

    let high_water = module.lower_var_high_water;
    let mut ctx = LowerCtx {
        workspace: ws,
        module,
        generics_in_scope,
        _next_var_id: high_water,
        placeholder_param: None,
        placeholder_forbidden: false,
        scope_probes: Default::default(),
    };

    let mut body = Body::default();

    // Register the signature's type-expr spans in the body's node lapper so
    // `type_at` / goto-definition still resolve on the edited fn's params +
    // return type. Lowered BODY-LOCAL (`TyPos::Body`), NOT into the module
    // `ty_arena`: `StaticTy` and `LocalTy` resolve identically for hover + goto
    // (they both read the `TypeExpr`'s nominal name -- see the bone-semantic
    // `hover_inbody_annotation` fixture), and the body arena is rebuilt wholesale
    // each edit, so this adds nothing to the leak-prone module arena. The
    // function's `param_decls` / `return_ty` keep their existing `Static` ids.
    if let Some(ret_ty) = &f.return_ty {
        lower_type_expr_local(&mut ctx, a, &mut body, a.te(ret_ty))?;
    }
    for p in f.params.iter() {
        lower_type_expr_local(&mut ctx, a, &mut body, a.te(&p.ty))?;
    }

    let (body_failed, _inline_body, _is_extern, _ffi_lib, _backend, mut errors) =
        lower_fn_body_into(&mut ctx, a, f, &mut body, &param_tys)?;
    ctx.module.lower_var_high_water = ctx._next_var_id;

    // Re-validate annotations on the body-only path too: the REPLACE below
    // would otherwise clear a still-present annotation violation recorded by
    // the last full lower.
    let lowered_annos = lower_annotations_with_ctx(&ctx, a, annotations);
    if let Err(e) = validate_annotations(AnnoPos::FnItem, &lowered_annos, &ctx.workspace.decorator_names) {
        errors.push(e);
    }
    for pd in ctx.module.functions[fid].param_decls.iter() {
        if let Err(e) = validate_annotations(AnnoPos::Param, &pd.annotations, &ctx.workspace.decorator_names) {
            errors.push(e);
        }
    }
    ctx.flush_scope_probes();

    // REPLACE this body's lowering errors (clears stale ones from a prior edit) --
    // the flat-Vec append was the lower-error-path leak + stale-diagnostic bug.
    if errors.is_empty() {
        ctx.module.lowering_errors.remove(&owner);
    } else {
        ctx.module.lowering_errors.insert(owner, errors);
    }

    // The signature is kept as-is EXCEPT its span: `shift_module_after_edit`
    // deliberately skips the edited fn's own span (it expected `build_function`
    // to replace the whole `Function`), so patch the new full-item span +
    // name span here -- otherwise `body_at` / navigation go stale on the next edit.
    let existing = &mut ctx.module.functions[fid];
    existing.span = f.span;
    existing.name_span = f.name.span;

    Ok(if body_failed {
        ctx.module.bodies.remove(&owner);
        false
    } else {
        ctx.module.insert_body(owner, body);
        true
    })
}

// Runtime marker keys: eq, inspect, deepcopy, and call dispatch duck-type on
// their presence, and `__rock_create_struct` overwrites `__ty` -- a user field
// with one of these names silently misbehaves, so reject at the declaration.
fn is_reserved_runtime_field(name: &str) -> bool {
    matches!(name, "__fn" | "__ty" | "__variant")
}

pub fn lower_struct_def(
    ctx: &mut LowerCtx,
    a: &Ast,
    s: &skull_parser::StructDef,
    reg_key: Ustr,
) -> Result<(), CompilerError> {
    reject_generic_bounds("a struct", &s.generics)?;
    validate_generic_defaults("a struct", &s.generics, a)?;
    let generic_var_ids = ctx.declare_generics(&s.generics);
    let mut body = Body::default();
    lower_generic_defaults_for_check(ctx, a, &mut body, &s.generics)?;

    let mut fields = Vec::new();
    let def_scope = body.alloc_root_scope();

    for field in s.fields.iter() {
        let ty = lower_type_expr(ctx, a, &mut body, a.te(&field.ty))?;
        let default = field
            .default
            .as_ref()
            .map(|e| lower_expr(ctx, a, &mut body, def_scope, a.e(e)))
            .transpose()?;

        if fields.iter().any(|prev: &hir::StructFieldDef| prev.name == field.name.name) {
            comp_error!(
                field.name.span,
                "Duplicate field `{}` in struct `{}`",
                field.name.name,
                s.name.name
            );
        }

        if is_reserved_runtime_field(field.name.name.as_str()) {
            comp_error!(
                field.name.span,
                "Reserved field name `{}` in struct `{}`: `__fn`, `__ty`, and `__variant` are runtime marker keys",
                field.name.name,
                s.name.name
            );
        }

        let (read_vis, write_vis) = if a.dialect == skull_parser::Dialect::V1 {
            let read_vis = match &field.vis {
                Some(v) => bone_core::modules::NsVisibility::from_parser(v),
                None => bone_core::modules::NsVisibility::Pub,
            };
            let write_vis = if field.set_private {
                bone_core::modules::NsVisibility::Private
            } else {
                read_vis.clone()
            };
            if field.set_private && !bone_core::modules::is_setter_audience_within_getter(&write_vis, &read_vis) {
                comp_error!(
                    field.name.span,
                    "write audience must be within read audience: field `{}` has `private(set)` but getter is already private",
                    field.name.name
                );
            }
            (read_vis, write_vis)
        } else {
            (bone_core::modules::NsVisibility::Pub, bone_core::modules::NsVisibility::Pub)
        };

        let field_annotations = lower_annotations_with_ctx(ctx, a, &field.annotations);
        validate_annotations(AnnoPos::StructField, &field_annotations, &ctx.workspace.decorator_names)?;
        fields.push(hir::StructFieldDef {
            name: field.name.name,
            ty,
            span: field.name.span,
            has_default: field.default.is_some(),
            annotations: field_annotations,
            is_embed: field.is_embed,
            read_vis,
            write_vis,
        });

        if let Some(default) = default {
            body.field_defaults.insert(field.name.name, default);
        }
    }

    let idx = ctx.module.struct_by_name[&reg_key];
    ctx.module.structs[idx].fields = fields;
    ctx.module.structs[idx].generics = generic_var_ids;

    ctx.module.insert_body(BodyOwner::StructDefaults(idx), body);

    ctx.module.make_lapper(
        s.span.offset_range.0,
        s.span.offset_range.1,
        BodyOwner::StructDefaults(idx),
    );

    Ok(())
}

pub fn lower_enum_def(
    ctx: &mut LowerCtx,
    a: &Ast,
    e: &skull_parser::EnumDef,
    reg_key: Ustr,
) -> Result<(), CompilerError> {
    reject_generic_bounds("an enum", &e.generics)?;
    validate_generic_defaults("an enum", &e.generics, a)?;
    let generic_var_ids = ctx.declare_generics(&e.generics);
    let mut body = Body::default();
    lower_generic_defaults_for_check(ctx, a, &mut body, &e.generics)?;

    let mut variants = Vec::new();

    for v in e.variants.iter() {
        use skull_parser::EnumVariantKind;

        let payload = match &v.kind {
            EnumVariantKind::Unit => VariantPayload::Unit,
            EnumVariantKind::Tuple(fs) => VariantPayload::Tuple(
                fs.iter()
                    .map(|f| lower_type_expr(ctx, a, &mut body, a.te(f)))
                    .try_vec()?,
            ),
            EnumVariantKind::Record(fs) => {
                let rec: Vec<hir::StructFieldDef> = fs
                    .iter()
                    .map(|f| {
                        let annos = lower_annotations_with_ctx(ctx, a, &f.annotations);
                        validate_annotations(AnnoPos::EnumVariantField, &annos, &ctx.workspace.decorator_names)?;
                        if f.is_embed {
                            comp_error!(
                                f.name.span,
                                "`embed` is not allowed on an enum variant field `{}`; embed inlines leaves into a struct owner",
                                f.name.name
                            );
                        }
                        Ok(hir::StructFieldDef {
                            name: f.name.name,
                            ty: lower_type_expr(ctx, a, &mut body, a.te(&f.ty))?,
                            span: f.name.span,
                            has_default: false,
                            annotations: annos,
                            is_embed: false,
                            read_vis: bone_core::modules::NsVisibility::Pub,
                            write_vis: bone_core::modules::NsVisibility::Pub,
                        })
                    })
                    .try_vec()?;
                for (i, f) in rec.iter().enumerate() {
                    if rec[..i].iter().any(|prev| prev.name == f.name) {
                        comp_error!(
                            f.span,
                            "Duplicate field `{}` in variant `{}` of enum `{}`",
                            f.name,
                            v.name.name,
                            e.name.name
                        );
                    }
                    if is_reserved_runtime_field(f.name.as_str()) {
                        comp_error!(
                            f.span,
                            "Reserved field name `{}` in variant `{}` of enum `{}`: `__fn`, `__ty`, and `__variant` are runtime marker keys",
                            f.name,
                            v.name.name,
                            e.name.name
                        );
                    }
                }
                VariantPayload::Record(rec)
            }
        };

        if variants.iter().any(|prev: &EnumVariantDef| prev.name == v.name.name) {
            comp_error!(
                v.name.span,
                "Duplicate variant `{}` in enum `{}`",
                v.name.name,
                e.name.name
            );
        }

        variants.push(EnumVariantDef {
            name: v.name.name,
            name_span: v.name.span,
            payload,
        });
    }

    let idx = ctx.module.enums_by_name[&reg_key];
    ctx.module.enums[idx].variants = variants;
    ctx.module.enums[idx].generics = generic_var_ids;

    ctx.module.insert_body(BodyOwner::Enum(idx), body);

    ctx.module.make_lapper(
        e.span.offset_range.0,
        e.span.offset_range.1,
        BodyOwner::Enum(idx),
    );

    Ok(())
}

pub fn lower_newtype_def(
    ctx: &mut LowerCtx,
    a: &Ast,
    n: &skull_parser::NewtypeDef,
    reg_key: Ustr,
) -> Result<(), CompilerError> {
    reject_generic_bounds("a newtype", &n.generics)?;
    validate_generic_defaults("a newtype", &n.generics, a)?;
    let generic_var_ids = ctx.declare_generics(&n.generics);
    let mut body = Body::default();
    lower_generic_defaults_for_check(ctx, a, &mut body, &n.generics)?;

    let idx = ctx.module.newtypes_by_name[&reg_key];

    ctx.module.newtypes[idx].wrapped = lower_type_expr(ctx, a, &mut body, a.te(&n.wrapped))?;
    ctx.module.newtypes[idx].generics = generic_var_ids;

    ctx.module.insert_body(BodyOwner::Newtype(idx), body);

    ctx.module.make_lapper(
        n.span.offset_range.0,
        n.span.offset_range.1,
        BodyOwner::Newtype(idx),
    );

    Ok(())
}

fn lower_type_expr_helper(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    pos: TyPos,
    orig_te: &skull_parser::TypeExprData,
) -> Result<hir::TypeExpr, CompilerError> {
    let id = lower_type_expr_in(ctx, a, body, pos, orig_te)?;
    // let te = match id {
    //     TyId::Static(module_id, idx) if module_id == ctx.module.module_id() => {
    //         ctx.module.ty_arena[idx]
    //     }
    //     TyId::Static(module_id, idx) => ctx.workspace.modules[&module_id].ty_arena[idx],
    //
    //     TyId::Local(_) => comp_error!(orig_te.span, "Created local TyId while lowering"),
    // };

    ctx.type_node(body, id, orig_te.span)
}

fn lower_exprs<'a>(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    scope: Idx<Scope>,
    exprs: impl Iterator<Item = &'a Idx<ExprData>>,
) -> Result<IdxRange<Expr>, CompilerError> {
    let mut lowered_args = Vec::new();

    for e in exprs {
        let e = a.e(e);
        lowered_args.push((lower_expr_value(ctx, a, body, scope, e)?, e));
    }

    // TODO: lets also figure out this min/max thing as an abstraction maybe?
    let mut min = None;
    let mut max = None;

    for (v, e) in lowered_args {
        let id = alloc_value(ctx, a, body, e, v);
        min = min.or(Some(id));
        max = Some(id);
    }

    let args_range = if let (Some(min), Some(max)) = (min, max) {
        IdxRange::new_inclusive(min..=max)
    } else {
        IdxRange::new_empty()
    };

    Ok(args_range)
}

fn lower_statements(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    scope: Idx<Scope>,
    statements: &[Idx<StatementData>],
) -> Result<(IdxRange<Statement>, Option<Idx<Expr>>), CompilerError> {
    let mut lowered = Vec::new();
    let mut tail = None;

    let tail_pos = statements
        .iter()
        .map(|s| a.s(s))
        .rposition(|s| !matches!(s.kind, skull_parser::StatementKind::Comment(_)))
        .filter(|&i| {
            matches!(
                a.s(&statements[i]).kind,
                skull_parser::StatementKind::Expression(_)
            )
        });

    for (i, s) in statements.iter().enumerate() {
        let s = a.s(s);

        if Some(i) == tail_pos {
            if let skull_parser::StatementKind::Expression(e) = &s.kind {
                let e = a.e(e);
                tail = Some((lower_expr_value(ctx, a, body, scope, e)?, e));
            }
        } else {
            lowered.push((lower_statement_value(ctx, a, body, scope, s)?, s.span));
        }
    }

    // TODO: lets also figure out this min/max thing as an abstraction maybe?
    let mut min = None;
    let mut max = None;

    let tail = if let Some((t, e)) = tail {
        Some(alloc_value(ctx, a, body, e, t))
    } else {
        None
    };

    for (s, span) in lowered {
        let id = body.alloc_statement(s, span);
        min = min.or(Some(id));
        max = Some(id);
    }

    let args_range = if let (Some(min), Some(max)) = (min, max) {
        IdxRange::new_inclusive(min..=max)
    } else {
        IdxRange::new_empty()
    };

    Ok((args_range, tail))
}

pub fn lower_global(
    ctx: &mut LowerCtx,
    a: &Ast,
    g: &skull_parser::GlobalDef,
    annotations: &[skull_parser::Annotation],
    reg_key: Ustr,
) -> Result<(), CompilerError> {
    let name = reg_key;
    let mut body = Body::default();
    let state_metadata = lower_state_metadata(ctx, a, annotations)?;

    let ty = match &g.ty {
        Some(te) => lower_type_expr(ctx, a, &mut body, a.te(te))?,
        None => comp_error!(
            g.span,
            "Global variable {} must have an explicit type annotation",
            name
        ),
    };

    let value = match &g.value {
        Some(e) => {
            let scope = body.alloc_root_scope();
            Some(lower_expr(ctx, a, &mut body, scope, a.e(e))?)
        }

        None => None,
    };

    body.root = value;

    let idx = ctx.module.globals.alloc(GlobalDef {
        name,
        name_span: g.name.span,
        ty,
        span: g.span,
        is_const: g.is_const,
        state_metadata,
    });

    if ctx.module.global_by_name.insert(name, idx).is_some() {
        comp_error!(g.span, "Duplicate global variable definition: `{}`", name);
    }

    ctx.module.insert_body(BodyOwner::Global(idx), body);

    ctx.module.make_lapper(
        g.span.offset_range.0,
        g.span.offset_range.1,
        BodyOwner::Global(idx),
    );

    Ok(())
}

/// The default ownership of a `global` that says nothing itself: the file's
/// `@memory(<class>)` header, else the owning package's `[language] memory`
/// (`docs/state-ownership-plan.md` R2). `game` is today's behavior, so a
/// package that declares nothing is unchanged.
fn metadata_for_memory_class(class: skull_parser::MemoryClass) -> StateMetadata {
    use skull_parser::MemoryClass;
    let ownership = match class {
        MemoryClass::Game => StateOwnership::Session,
        MemoryClass::Platform => StateOwnership::HostProjection,
        MemoryClass::Debug => StateOwnership::RunnerDiagnostic,
    };
    let (world, realization) = match ownership {
        StateOwnership::Session => (StateWorldPolicy::Include, StateRealization::FreshSession),
        StateOwnership::HostProjection => (
            StateWorldPolicy::Exclude,
            StateRealization::RebuildHostProjection,
        ),
        StateOwnership::RunnerDiagnostic => {
            (StateWorldPolicy::Exclude, StateRealization::RunnerRebind)
        }
        StateOwnership::Unsupported => (StateWorldPolicy::Exclude, StateRealization::Reject),
    };
    StateMetadata { ownership, world, realization }
}

fn lower_state_metadata(
    ctx: &LowerCtx,
    ast: &Ast,
    annotations: &[skull_parser::Annotation],
) -> Result<StateMetadata, CompilerError> {
    let unit_default = metadata_for_memory_class(
        ast.memory_header.map(|h| h.class).unwrap_or(ast.memory_class),
    );
    let annotations = lower_annotations_with_ctx(ctx, ast, annotations);
    let Some(annotation) = annotations.iter().find(|annotation| annotation.is_builtin("state")) else {
        return Ok(unit_default);
    };
    if annotations
        .iter()
        .filter(|candidate| candidate.is_builtin("state"))
        .count()
        != 1
    {
        comp_error!(annotation.span, "a global may have only one `@state` annotation");
    }

    let mut metadata = StateMetadata::default();
    let mut world_explicit = false;
    let mut realization_explicit = false;
    for arg in &annotation.args {
        let key = arg.name.as_ref().expect("state annotation arguments were validated");
        let value = match &arg.value {
            bone_core::hir::AnnoValue::Ident(value) => value.name.as_str(),
            _ => unreachable!("state annotation values were validated"),
        };
        match key.name.as_str() {
            "ownership" => {
                metadata.ownership = match value {
                    "session" => StateOwnership::Session,
                    "host_projection" => StateOwnership::HostProjection,
                    "runner_diagnostic" => StateOwnership::RunnerDiagnostic,
                    "unsupported" => StateOwnership::Unsupported,
                    _ => comp_error!(
                        annotation.span,
                        "unknown StateImage ownership `{value}`"
                    ),
                };
            }
            "world" => {
                world_explicit = true;
                metadata.world = match value {
                    "include" => StateWorldPolicy::Include,
                    "boot_only" => StateWorldPolicy::BootOnly,
                    "exclude" => StateWorldPolicy::Exclude,
                    _ => comp_error!(annotation.span, "unknown StateImage world policy `{value}`"),
                };
            }
            "realization" => {
                realization_explicit = true;
                metadata.realization = match value {
                    "fresh_session" => StateRealization::FreshSession,
                    "rebuild_host_projection" => StateRealization::RebuildHostProjection,
                    "font" => StateRealization::Font,
                    "asset_cache" => StateRealization::AssetCache,
                    "asset_paths" => StateRealization::AssetPaths,
                    "process_window" => StateRealization::ProcessWindow,
                    "runner_reset_nil" => StateRealization::RunnerResetNil,
                    "runner_reset_int" => StateRealization::RunnerResetInt,
                    "runner_reset_bool" => StateRealization::RunnerResetBool,
                    "runner_reset_true" => StateRealization::RunnerResetTrue,
                    "runner_reset_string" => StateRealization::RunnerResetString,
                    "runner_rebind" => StateRealization::RunnerRebind,
                    "reject" => StateRealization::Reject,
                    _ => comp_error!(annotation.span, "unknown StateImage realization `{value}`"),
                };
            }
            _ => unreachable!("state annotation keys were validated"),
        }
    }
    if !world_explicit {
        metadata.world = match metadata.ownership {
            StateOwnership::Session => StateWorldPolicy::Include,
            StateOwnership::HostProjection
            | StateOwnership::RunnerDiagnostic
            | StateOwnership::Unsupported => StateWorldPolicy::Exclude,
        };
    }
    if !realization_explicit {
        metadata.realization = match metadata.ownership {
            StateOwnership::Session => StateRealization::FreshSession,
            StateOwnership::HostProjection => StateRealization::RebuildHostProjection,
            StateOwnership::RunnerDiagnostic => StateRealization::RunnerRebind,
            StateOwnership::Unsupported => StateRealization::Reject,
        };
    }
    Ok(metadata)
}

fn lower_pattern_value(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    scope: Idx<Scope>,
    pat: &skull_parser::Pattern,
) -> Result<Pattern, CompilerError> {
    // Modules-v1 qualified variant patterns resolve to the enum's DefKey and
    // lower exactly like the legacy two-segment forms.
    if let skull_parser::Pattern::PathVariant { path, args } = pat {
        let Some(frag) = ctx.v1_scope() else {
            return Err(CompilerError::new_no_location(
                "qualified patterns are modules-v1 grammar; this file is legacy".to_string(),
            ));
        };
        let use_path = bone_core::modules::UsePathRef {
            from_root: path.from_root,
            segments: path.segments.iter().map(|i| i.name).collect(),
        };
        let (enum_key, variant) = ctx.probed(|| resolve::resolve_variant_pattern(ctx.workspace, frag, &use_path))
            .map_err(|msg| CompilerError::new_no_location(msg))?;
        ctx.record_path(body, TyPos::Body, path, hir::SegmentTarget::Variant { enum_key, variant });
        let sub = match args {
            None => IdxRange::new_empty(),
            Some(pats) => {
                let vals = pats
                    .iter()
                    .map(|p| lower_pattern_value(ctx, a, body, scope, p))
                    .try_vec()?;
                alloc_contiguous(&mut body.pattern_arena, vals)
            }
        };
        return Ok(Pattern::EnumVariant {
            enum_name: Some(enum_key),
            enum_name_span: Some(path.span()),
            variant,
            variant_span: path.span(),
            sub,
        });
    }
    if let skull_parser::Pattern::PathRecord { path, fields, rest } = pat {
        let Some(frag) = ctx.v1_scope() else {
            return Err(CompilerError::new_no_location(
                "qualified patterns are modules-v1 grammar; this file is legacy".to_string(),
            ));
        };
        let use_path = bone_core::modules::UsePathRef {
            from_root: path.from_root,
            segments: path.segments.iter().map(|i| i.name).collect(),
        };
        let (enum_key, variant) = ctx.probed(|| resolve::resolve_variant_pattern(ctx.workspace, frag, &use_path))
            .map_err(|msg| CompilerError::new_no_location(msg))?;
        ctx.record_path(body, TyPos::Body, path, hir::SegmentTarget::Variant { enum_key, variant });
        let mut entries = Vec::with_capacity(fields.len());
        for (fname, subpat) in fields.iter() {
            let p = lower_pattern_value(ctx, a, body, scope, subpat)?;
            let p = body.pattern_arena.alloc(p);
            entries.push(RecordPatField {
                name: fname.name,
                name_span: fname.span,
                pat: p,
            });
        }
        let fields_range = alloc_contiguous(&mut body.record_pat_field_arena, entries);
        return Ok(Pattern::Record {
            enum_name: Some(enum_key),
            enum_name_span: Some(path.span()),
            name: Some(variant),
            name_span: Some(path.span()),
            fields: fields_range,
            rest: *rest,
        });
    }

    Ok(match pat {
        skull_parser::Pattern::Wildcard => Pattern::Wildcard,
        // Handled by the early returns above; the match stays exhaustive.
        skull_parser::Pattern::PathVariant { .. } | skull_parser::Pattern::PathRecord { .. } => {
            unreachable!("qualified patterns returned above")
        }

        skull_parser::Pattern::Ident(id) => {
            let b = ctx.alloc_binding(
                body,
                BindingInfo {
                    name: id.name,
                    kind: hir::BindingKind::Let,
                    declared_ty: None,
                    span: id.span,
                },
                id.span,
            );

            body.scope_arena[scope].bindings.insert(id.name, b);

            Pattern::Bind(b)
        }

        skull_parser::Pattern::EnumVariant {
            enum_name,
            variant_name,
            args,
        } => {
            let ename = match enum_name {
                Some(e) => Some(ctx.variant_enum_key(e.name, e.span)?.unwrap_or(e.name)),
                None => None,
            };

            let sub = match args {
                None => IdxRange::new_empty(),

                Some(pats) => {
                    let vals = pats
                        .iter()
                        .map(|p| lower_pattern_value(ctx, a, body, scope, p))
                        .try_vec()?;
                    alloc_contiguous(&mut body.pattern_arena, vals)
                }
            };

            Pattern::EnumVariant {
                enum_name: ename,
                enum_name_span: enum_name.as_ref().map(|e| e.span),
                variant: variant_name.name,
                variant_span: variant_name.span,
                sub,
            }
        }

        skull_parser::Pattern::Range {
            start,
            end,
            inclusive,
        } => {
            let start = start
                .as_ref()
                .map(|e| lower_expr(ctx, a, body, scope, a.e(e)))
                .transpose()?;
            let end = end
                .as_ref()
                .map(|e| lower_expr(ctx, a, body, scope, a.e(e)))
                .transpose()?;

            Pattern::Range {
                start,
                end,
                inclusive: *inclusive,
            }
        }

        skull_parser::Pattern::Literal(expr) => {
            let e = lower_expr(ctx, a, body, scope, a.e(expr))?;
            Pattern::Literal(e)
        }

        skull_parser::Pattern::Record {
            enum_name,
            name,
            fields,
            rest,
        } => {
            let mut entries = Vec::with_capacity(fields.len());

            for (fname, subpat) in fields.iter() {
                let pat = lower_pattern_value(ctx, a, body, scope, subpat)?;
                let pat = body.pattern_arena.alloc(pat);
                entries.push(RecordPatField {
                    name: fname.name,
                    name_span: fname.span,
                    pat,
                });
            }

            let fields = alloc_contiguous(&mut body.record_pat_field_arena, entries);

            let enum_key = match enum_name {
                Some(e) => Some(ctx.variant_enum_key(e.name, e.span)?.unwrap_or(e.name)),
                None => None,
            };
            Pattern::Record {
                enum_name: enum_key,
                enum_name_span: enum_name.as_ref().map(|e| e.span),
                name: name.as_ref().map(|n| n.name),
                name_span: name.as_ref().map(|n| n.span),
                fields,
                rest: *rest,
            }
        }

        skull_parser::Pattern::Tuple(elems) => {
            let vals = elems
                .iter()
                .map(|p| lower_pattern_value(ctx, a, body, scope, p))
                .try_vec()?;

            let elements = alloc_contiguous(&mut body.pattern_arena, vals);

            Pattern::Tuple { elements }
        }

        skull_parser::Pattern::Or(alts) => {
            let vals = alts
                .iter()
                .map(|p| lower_pattern_value(ctx, a, body, scope, p))
                .try_vec()?;

            let alternatives = alloc_contiguous(&mut body.pattern_arena, vals);

            Pattern::Or { alternatives }
        }
    })
}

fn lower_if_let_clauses(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    scope: Idx<Scope>,
    clauses: &[IfLetClause],
    then_block: &Idx<BlockData>,
    else_block: &Option<Idx<BlockData>>,
) -> Result<Idx<Expr>, CompilerError> {
    let Some((clause, rest)) = clauses.split_first() else {
        let e = lower_block(ctx, a, body, scope, then_block)?;
        return Ok(ctx.alloc_block_expr(body, e, a.b(then_block)));
    };

    let else_branch = match else_block {
        Some(blk) => {
            let e = lower_block(ctx, a, body, scope, blk)?;
            Some(ctx.alloc_block_expr(body, e, a.b(blk)))
        }

        None => None,
    };

    let node = match clause {
        IfLetClause::Binding { pattern, value } => {
            let value = lower_expr(ctx, a, body, scope, a.e(value))?;
            let then_scope = body.alloc_child_scope(scope);

            let pat = lower_pattern_value(ctx, a, body, then_scope, pattern)?;

            let pattern = body.pattern_arena.alloc(pat);
            let then_branch =
                lower_if_let_clauses(ctx, a, body, then_scope, rest, then_block, else_block)?;

            Expr::IfLet {
                pattern,
                value,
                then_branch,
                else_branch,
            }
        }

        IfLetClause::Condition(cond) => {
            let cond = lower_expr(ctx, a, body, scope, a.e(cond))?;
            let then_branch =
                lower_if_let_clauses(ctx, a, body, scope, rest, then_block, else_block)?;

            Expr::If {
                cond,
                then_branch,
                else_branch,
            }
        }
    };

    Ok(ctx.alloc_expr(body, node, a.b(then_block).span))
}

fn lower_interp_parts(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    scope: Idx<Scope>,
    parts: &[skull_parser::InterpolatedPart],
) -> Result<IdxRange<InterpPart>, CompilerError> {
    use skull_parser::InterpolatedPart as P;
    let mut ids = Vec::with_capacity(parts.len());

    for part in parts {
        let hir = match part {
            P::Literal(s) => InterpPart::Literal(*s),

            P::Expr(e, spec) => {
                let e = a.e(e);
                let val = lower_expr_value(ctx, a, body, scope, e)?;
                let expr = alloc_value(ctx, a, body, e, val);

                let spec = match spec {
                    Some(s) => {
                        let width_expr = match s.width_expr {
                            Some(we) => {
                                let we = a.e(&we);
                                let v = lower_expr_value(ctx, a, body, scope, we)?;
                                Some(alloc_value(ctx, a, body, we, v))
                            }

                            None => None,
                        };

                        let precision_expr = match s.precision_expr {
                            Some(pe) => {
                                let pe = a.e(&pe);
                                let v = lower_expr_value(ctx, a, body, scope, pe)?;
                                Some(alloc_value(ctx, a, body, pe, v))
                            }

                            None => None,
                        };

                        Some(InterpSpec {
                            precision: s.precision,
                            precision_expr,

                            width: s.width,
                            width_expr,

                            alignment: s.alignment,
                            fill: s.fill,
                            sign: s.sign,
                            alternate: s.alternate,
                            zero_pad: s.zero_pad,
                            grouping: s.grouping,
                            format_type: s.format_type,
                        })
                    }

                    None => None,
                };

                InterpPart::Value { expr, spec }
            }
        };

        ids.push(body.interp_part_arena.alloc(hir));
    }

    Ok(match (ids.first(), ids.last()) {
        (Some(min), Some(max)) => IdxRange::new_inclusive(*min..=*max),
        _ => IdxRange::new_empty(),
    })
}

pub fn lower_decorator_def(
    ctx: &mut LowerCtx,
    a: &Ast,
    d: &skull_parser::DecoratorDef,
    annotations: &[skull_parser::Annotation],
    def_key: Ustr,
) -> Result<Idx<DecoratorDef>, CompilerError> {
    ctx.generics_in_scope.clear();
    let mut body = Body::default();

    let mut params = Vec::new();
    let mut param_decls = Vec::new();
    let mut bindings = FxHashMap::default();

    for p in d.params.iter() {
        let ty_ref = lower_type_expr(ctx, a, &mut body, a.te(&p.ty))?;

        let b_idx = ctx.alloc_binding(
            &mut body,
            BindingInfo {
                name: p.name.name,
                kind: BindingKind::Param,
                declared_ty: Some(ty_ref),
                span: p.name.span,
            },
            p.span,
        );

        bindings.insert(p.name.name, b_idx);
        params.push(b_idx);
        let deco_param_annotations = lower_annotations_with_ctx(ctx, a, &p.annotations);
        validate_annotations(AnnoPos::DecoParam, &deco_param_annotations, &ctx.workspace.decorator_names)?;
        param_decls.push(ParamDecl {
            name: p.name.name,
            ty: ty_ref,
            has_default: p.default.is_some(),
            annotations: deco_param_annotations,
        });
    }

    let target_ty = {
        let idx = ctx.module.ty_arena.alloc(TypeExpr::App {
            ctor: TypeCtor::Nominal(ustr("__DecoTarget")),
            params: IdxRange::new_empty(),
        });

        TyId::Static(ctx.module.module_id(), idx)
    };

    let target = ctx.alloc_binding(
        &mut body,
        BindingInfo {
            name: ustr("target"),
            kind: BindingKind::Param,
            declared_ty: Some(target_ty),
            span: d.name.span,
        },
        d.span,
    );

    bindings.insert(ustr("target"), target);

    let params_range = match (params.first(), params.last()) {
        (Some(a), Some(b)) => IdxRange::new_inclusive(*a..=*b),
        _ => IdxRange::new_empty(),
    };

    let root_scope = body.alloc_root_scope_with_bindings(bindings);

    let mut param_defaults = Vec::new();
    for p in d.params.iter() {
        param_defaults.push(match &p.default {
            Some(expr) => Some(lower_expr(ctx, a, &mut body, root_scope, a.e(expr))?),
            None => None,
        });
    }

    let lowered_body = lower_block(ctx, a, &mut body, root_scope, &d.body)?;
    let body_e = ctx.alloc_block_expr(&mut body, lowered_body, a.b(&d.body));

    let num_required = d
        .params
        .iter()
        .filter(|p| p.default.is_none() && !p.is_variadic)
        .count();

    body.root = Some(body_e);
    body.root_scope = Some(root_scope);
    body.param_bindings = params_range;
    body.param_defaults = param_defaults;
    body.deco_target = Some(target);

    let idx = ctx.module.decorators.alloc(DecoratorDef {
        name: d.name.name,
        def_key,
        span: d.span,
        param_decls,
        num_required,
        annotations: lower_annotations_with_ctx(ctx, a, annotations),
    });

    ctx.module.insert_body(BodyOwner::Decorator(idx), body);

    ctx.module.make_lapper(
        d.span.offset_range.0,
        d.span.offset_range.1,
        BodyOwner::Decorator(idx),
    );

    Ok(idx)
}

pub fn lower_annotations(ast: &Ast, anns: &[skull_parser::Annotation]) -> Vec<hir::Annotation> {
    anns.iter()
        .map(|a| hir::Annotation {
            span: a.span,
            name: a.name.clone(),
            args: a
                .args
                .iter()
                .map(|arg| hir::AnnoArg {
                    name: arg.name.clone(),
                    value: classify_anno_value(ast, arg.value),
                })
                .collect(),
            resolved_name: None,
            resolved_ident_list: None,
        })
        .collect()
}

/// Lower annotation syntax and resolve v1 decorator names through the current
/// fragment. Compiler-owned metadata intentionally stays unresolved so its
/// raw spelling remains the source of builtin behavior.
pub fn lower_annotations_with_ctx(
    ctx: &LowerCtx,
    ast: &Ast,
    anns: &[skull_parser::Annotation],
) -> Vec<hir::Annotation> {
    let mut lowered = lower_annotations(ast, anns);
    let Some(frag) = ctx.v1_scope() else {
        return lowered;
    };
    for anno in &mut lowered {
        if let Ok(resolved) = ctx.probed(|| resolve::resolve_bare(ctx.workspace, Some(frag), anno.name.name)) {
            anno.resolved_name = Some(resolved.key);
        }
        let is_wire_role = anno.name.name.as_str() == "wire"
            || anno.resolved_name.is_some_and(|key| {
                ctx.workspace
                    .defs
                    .get(&key)
                    .is_some_and(|meta| meta.kind == bone_core::modules::DefKind::Decorator && meta.name.as_str() == "wire")
            });
        if !is_wire_role {
            continue;
        }
        let Some(names) = anno.args.iter().find_map(|arg| {
            (arg.name.as_ref().is_some_and(|name| name.name.as_str() == "onchange"))
                .then(|| match &arg.value {
                    hir::AnnoValue::IdentList(names) => Some(names),
                    _ => None,
                })
                .flatten()
        }) else {
            continue;
        };
        let mut resolved = Vec::with_capacity(names.len());
        for name in names {
            let Ok(identity) = ctx.probed(|| resolve::resolve_bare(ctx.workspace, Some(frag), name.name)) else {
                resolved.clear();
                break;
            };
            resolved.push(identity.key);
        }
        if resolved.len() == names.len() {
            anno.resolved_ident_list = Some(resolved);
        }
    }
    lowered
}

fn classify_anno_value(a: &Ast, e: Idx<ExprData>) -> hir::AnnoValue {
    use skull_parser::ExprKind;

    match &a.e(&e).kind {
        ExprKind::Int(i, _) => hir::AnnoValue::Int(*i),
        ExprKind::Float(f) => hir::AnnoValue::Float(f.0),
        ExprKind::Bool(b) => hir::AnnoValue::Bool(*b),
        ExprKind::String(s) => hir::AnnoValue::String(*s),
        ExprKind::Ident(id) => hir::AnnoValue::Ident(id.clone()),
        ExprKind::ArrayLiteral(items) => {
            let names = items
                .iter()
                .map(|item| match &a.e(item).kind {
                    ExprKind::Ident(id) => Some(id.clone()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>();
            names.map_or(hir::AnnoValue::Other, hir::AnnoValue::IdentList)
        }

        ExprKind::UnaryMinus(inner) => match &a.e(inner).kind {
            ExprKind::Int(i, _) => hir::AnnoValue::Int(-*i),
            ExprKind::Float(f) => hir::AnnoValue::Float(-f.0),
            _ => hir::AnnoValue::Other,
        },

        _ => hir::AnnoValue::Other,
    }
}

fn lvalue_has_safe_nav(a: &Ast, e: &Idx<ExprData>) -> bool {
    use skull_parser::ExprKind;

    match &a.e(e).kind {
        ExprKind::SafeNavigation(..) => true,

        ExprKind::FieldAccess(o, _) | ExprKind::Index(o, _) | ExprKind::Paren(o) => {
            lvalue_has_safe_nav(a, o)
        }

        _ => false,
    }
}

fn pl_is_us(a: &Ast, e: &Idx<ExprData>) -> bool {
    matches!(&a.e(e).kind, skull_parser::ExprKind::Ident(id) if id.name == "_")
}

fn pl_chain(a: &Ast, e: &Idx<ExprData>) -> bool {
    use skull_parser::ExprKind;

    match &a.e(e).kind {
        ExprKind::Ident(id) => id.name == "_",

        ExprKind::Paren(i)
        | ExprKind::FieldAccess(i, _)
        | ExprKind::SafeNavigation(i, _)
        | ExprKind::QuestionMark(i)
        | ExprKind::Not(i)
        | ExprKind::UnaryMinus(i)
        | ExprKind::BitNot(i)
        | ExprKind::PreIncrement(i)
        | ExprKind::PostIncrement(i)
        | ExprKind::PreDecrement(i)
        | ExprKind::PostDecrement(i)
        | ExprKind::As(i, _)
        | ExprKind::Bitcast(i, _)
        | ExprKind::TypeAscription(i, _)
        | ExprKind::ParenAscription(i, _) => pl_chain(a, i),

        ExprKind::BinOp { left, right, .. } => pl_chain(a, left) || pl_chain(a, right),

        ExprKind::FunctionCall { func, args, syntax, .. } => {
            if matches!(syntax, skull_parser::CallSyntax::Binary(_)) {
                args.iter().any(|x| pl_chain(a, &x.value))
            } else {
                pl_chain(a, func)
            }
        }

        ExprKind::Index(o, _) => pl_chain(a, o),
        _ => false,
    }
}

fn is_placeholder_lambda(a: &Ast, e: &ExprData) -> bool {
    use skull_parser::ExprKind;

    match &e.kind {
        ExprKind::BinOp { left, right, .. } => pl_chain(a, left) || pl_chain(a, right),
        ExprKind::FieldAccess(o, _) | ExprKind::SafeNavigation(o, _) => pl_chain(a, o),

        ExprKind::Index(o, i) => pl_is_us(a, o) || pl_is_us(a, i),

        ExprKind::FunctionCall { func, args, syntax, .. } => {
            if matches!(syntax, skull_parser::CallSyntax::Binary(_)) {
                args.iter().any(|x| pl_chain(a, &x.value))
            } else {
                pl_chain(a, func) || args.iter().any(|x| pl_is_us(a, &x.value))
            }
        }

        ExprKind::Paren(i) => is_placeholder_lambda(a, a.e(i)),

        ExprKind::UnaryMinus(i)
        | ExprKind::Not(i)
        | ExprKind::BitNot(i)
        | ExprKind::QuestionMark(i)
        | ExprKind::As(i, _)
        | ExprKind::TypeAscription(i, _)
        | ExprKind::ParenAscription(i, _) => pl_chain(a, i),

        ExprKind::If { branches, .. } => branches.iter().any(|(c, _)| pl_is_us(a, c)),

        _ => false,
    }
}

fn lower_placeholder_lambda(
    ctx: &mut LowerCtx,
    a: &Ast,
    body: &mut Body,
    scope: Idx<Scope>,
    e: &ExprData,
) -> Result<Expr, CompilerError> {
    let lambda_scope = body.alloc_child_scope(scope);
    let param = ctx.alloc_binding(
        body,
        BindingInfo {
            name: ustr("_"),
            kind: BindingKind::Param,
            declared_ty: None,
            span: e.span,
        },
        e.span,
    );

    let prev = ctx.placeholder_param.replace(param);

    let body_val = lower_expr_value(ctx, a, body, lambda_scope, e);
    ctx.placeholder_param = prev;

    let body_idx = alloc_value(ctx, a, body, e, body_val?);

    Ok(Expr::Lambda {
        params: IdxRange::new_inclusive(param..=param),
        body: body_idx,
    })
}
