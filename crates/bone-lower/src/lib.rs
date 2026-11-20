pub mod resolve;
#[cfg(test)]
mod scope_probe_tests;

use skull_parser::{
    Annotation, Ast, BlockData, DeferKind, ExprData, ExprKind, FunctionDef, IfLetClause,
    StatementData, StatementKind, TokenKind, TypeExprData,
};
use ustr::ustr;

use bone_core::{
    annotations::{AnnoPos, json_field_name, validate_annotations},
    hir::{
        AsKind, BindingInfo, BindingKind, Callee, DecoratorDef, EnumVariantDef, Expr, Function,
        GlobalDef, InterpPart, InterpSpec, MatchArm, ParamDecl, Pattern, RecordPatField, Scope,
        Statement, StructLitField, TyId, TypeCtor, TypeExpr, VarId, VariantArgs, VariantPayload,
    },
    prelude::*,
};

#[derive(Copy, Clone, Debug)]
pub(crate) enum TyPos {
    /// Interface position (signatures, fields, globals, enum/newtype):
    /// module ty_arena, TyId::Static - what other modules may reference
    Iface,
    /// Body position (let / as / ascription / lambda annotations):
    /// Body.lowered_tys, TyId::Local - dies with the body
    Body,
}

pub fn record_use_paths(ctx: &mut LowerCtx) {
    let a = ctx.ast;
    if ctx.workspace.skip_session_facts {
        return;
    }
    let root = ctx.fragment_scope().clone();
    let mut refs = Vec::new();
    record_use_items(ctx, &a.items, &root.module_path, &root, &mut refs);
    ctx.module.path_segments.extend(refs);
}

fn record_use_items(
    ctx: &LowerCtx,
    items: &[skull_parser::Item],
    module_path: &[Ustr],
    root: &bone_core::modules::FragmentScope,
    out: &mut Vec<hir::PathSegmentRef>,
) {
    let mut frag = root.clone();
    frag.module_path = module_path.to_vec();
    for item in items {
        match &item.kind {
            skull_parser::ItemKind::Use(decl) => {
                ctx.use_tree_refs(&frag, &decl.tree, &[], false, out);
            }
            skull_parser::ItemKind::Module(module) => {
                let mut child = module_path.to_vec();
                child.push(module.name.name);
                record_use_items(ctx, &module.items, &child, root, out);
            }
            _ => {}
        }
    }
}

fn member_segment_target(target: &resolve::MemberTarget) -> hir::SegmentTarget {
    match *target {
        resolve::MemberTarget::Global(key)
        | resolve::MemberTarget::Callable(key)
        | resolve::MemberTarget::Type(key) => hir::SegmentTarget::Def(key),
        resolve::MemberTarget::EnumVariant { enum_key, variant } => {
            hir::SegmentTarget::Variant { enum_key, variant }
        }
    }
}

pub(crate) fn alloc_ty(ctx: &mut LowerCtx, pos: TyPos, node: TypeExpr) -> TyId {
    match pos {
        TyPos::Iface => TyId::Static(ctx.module.module_id(), alloc_ty_raw(ctx, pos, node)),
        TyPos::Body => TyId::Local(alloc_ty_raw(ctx, pos, node)),
    }
}

pub(crate) fn alloc_ty_raw(ctx: &mut LowerCtx, pos: TyPos, node: TypeExpr) -> Idx<TypeExpr> {
    match pos {
        TyPos::Iface => ctx.module.ty_arena.alloc(node),
        TyPos::Body => ctx.body.lowered_tys.alloc(node),
    }
}

pub(crate) fn alloc_tys_contiguous(
    ctx: &mut LowerCtx,
    pos: TyPos,
    nodes: impl IntoIterator<Item = TypeExpr>,
) -> IdxRange<TypeExpr> {
    match pos {
        TyPos::Iface => alloc_contiguous(&mut ctx.module.ty_arena, nodes),
        TyPos::Body => alloc_contiguous(&mut ctx.body.lowered_tys, nodes),
    }
}

/// Lookup cache for one module lowering; probes are recorded on cache misses.
#[derive(Debug, Default)]
pub(crate) struct ResolutionCache {
    bare: std::cell::RefCell<FxHashMap<Ustr, Result<resolve::Resolved, String>>>,
    callee: std::cell::RefCell<FxHashMap<Ustr, Result<resolve::CalleeFamily, String>>>,
    optional: std::cell::RefCell<FxHashMap<Ustr, Result<Option<resolve::CalleeFamily>, String>>>,
}

fn use_path_ref(path: &skull_parser::Path) -> bone_core::modules::UsePathRef {
    bone_core::modules::UsePathRef {
        from_root: path.from_root,
        segments: path.segments.iter().map(|i| i.name).collect(),
    }
}

fn variant_path_spans(path: &skull_parser::Path) -> (Span, Span) {
    let enum_segment = path
        .segments
        .get(
            path.segments
                .len()
                .checked_sub(2)
                .expect("variant path has a qualifier"),
        )
        .expect("variant path has an enum segment")
        .span;
    let variant_segment = path
        .segments
        .last()
        .expect("variant path has a variant segment")
        .span;
    (enum_segment, variant_segment)
}

#[derive(Debug)]
pub struct LowerCtx<'w, 'm> {
    pub workspace: &'w Workspace,
    pub module: &'m mut hir::Module,
    pub ast: &'w Ast,
    pub(crate) body: Body,
    pub(crate) generics_in_scope: FxHashMap<Ustr, TyId>,

    pub _next_var_id: u32,

    pub(crate) placeholder_param: Option<Idx<BindingInfo>>,
    pub(crate) placeholder_forbidden: bool,

    pub(crate) scope_probes: std::cell::RefCell<Vec<bone_core::modules::ScopeProbe>>,
    pub(crate) resolutions: ResolutionCache,
    /// Already-evaluated place parts, keyed by source address, to avoid running
    /// them twice for compound assignment.
    pub(crate) place_temps: Vec<(usize, Idx<BindingInfo>)>,
    name_errors: std::cell::RefCell<Option<Vec<CompilerError>>>,
}

impl<'w, 'm> LowerCtx<'w, 'm> {
    pub fn new(
        workspace: &'w Workspace,
        module: &'m mut hir::Module,
        ast: &'w Ast,
        first_var: u32,
    ) -> Self {
        LowerCtx {
            workspace,
            module,
            ast,
            body: Body::default(),
            generics_in_scope: Default::default(),
            _next_var_id: first_var,
            placeholder_param: None,
            placeholder_forbidden: false,
            scope_probes: Default::default(),
            resolutions: Default::default(),
            place_temps: Vec::new(),
            name_errors: Default::default(),
        }
    }

    fn probed<R>(&self, lookup: impl FnOnce() -> R) -> R {
        if self.workspace.skip_session_facts {
            return lookup();
        }
        let (result, probes) = resolve::with_probes(lookup);
        self.scope_probes.borrow_mut().extend(probes);
        result
    }

    fn probed_cached<R: Clone>(
        &self,
        cache: &std::cell::RefCell<FxHashMap<Ustr, Result<R, String>>>,
        name: Ustr,
        lookup: impl FnOnce() -> Result<R, String>,
    ) -> Result<R, String> {
        if let Some(result) = cache.borrow().get(&name) {
            // The miss already recorded these dependencies for this module.
            return result.clone();
        }
        let result = self.probed(lookup);
        cache.borrow_mut().insert(name, result.clone());
        result
    }

    fn collect_name_errors<R>(
        &mut self,
        lower: impl FnOnce(&mut Self) -> R,
    ) -> (R, Vec<CompilerError>) {
        let outer = self.name_errors.replace(Some(Vec::new()));
        let result = lower(self);
        let errors = self.name_errors.replace(outer).unwrap_or_default();
        (result, errors)
    }

    fn recover_name(&self, e: CompilerError) -> Result<(), CompilerError> {
        match self.name_errors.borrow_mut().as_mut() {
            Some(errors) => {
                errors.push(e);
                Ok(())
            }
            None => Err(e),
        }
    }

    /// Record that `val` covers source bytes `start..stop`, for editor
    /// queries ([`Body::node_index`]); nothing in a one-shot pass.
    fn node_interval(&mut self, start: usize, stop: usize, val: hir::NodeRef) {
        if !self.workspace.skip_session_facts {
            self.body.make_lapper(start, stop, val);
        }
    }

    fn take_body(&mut self) -> Body {
        std::mem::take(&mut self.body)
    }

    pub fn flush_scope_probes(&mut self) {
        let probes = std::mem::take(self.scope_probes.get_mut());
        // Not `extend`: it reserves for every recorded probe, and lookups
        // repeat the same probes many times over.
        for probe in probes {
            self.module.scope_probes.insert(probe);
        }
    }

    pub fn fragment_scope(&self) -> &bone_core::modules::FragmentScope {
        &self.workspace.fragment_scopes[&self.module.file]
    }

    fn path_segment_refs(
        &self,
        from_root: bool,
        segments: &[skull_parser::Ident],
        last: hir::SegmentTarget,
    ) -> Vec<hir::PathSegmentRef> {
        let frag = self.fragment_scope();
        let names: Vec<Ustr> = segments.iter().map(|i| i.name).collect();
        let n = segments.len();
        let mut out = Vec::with_capacity(n);
        for (i, ident) in segments.iter().enumerate() {
            let target = if i + 1 == n {
                Some(last)
            } else if let (hir::SegmentTarget::Variant { enum_key, .. }, true) = (last, i + 2 == n)
            {
                Some(hir::SegmentTarget::Def(enum_key))
            } else {
                self.module_segment(frag, from_root, &names[..=i])
            };
            if let Some(target) = target {
                out.push(hir::PathSegmentRef {
                    span: ident.span,
                    target,
                    alias: false,
                });
            }
        }
        out
    }

    fn module_segment(
        &self,
        frag: &bone_core::modules::FragmentScope,
        from_root: bool,
        prefix: &[Ustr],
    ) -> Option<hir::SegmentTarget> {
        let prefix = bone_core::modules::UsePathRef {
            from_root,
            segments: prefix.to_vec(),
        };
        self.probed(|| resolve::resolve_path_to_module(self.workspace, frag, &prefix))
            .ok()
            .map(|(package, path)| hir::SegmentTarget::Module {
                package,
                node: resolve::namespace_key(self.workspace, package, &path),
            })
    }

    fn record_path(&mut self, pos: TyPos, path: &skull_parser::Path, last: hir::SegmentTarget) {
        if self.workspace.skip_session_facts {
            return;
        }
        let refs = self.path_segment_refs(path.from_root, &path.segments, last);
        match pos {
            TyPos::Body => self.body.path_segments.extend(refs),
            TyPos::Iface => self.module.path_segments.extend(refs),
        }
    }

    fn use_tree_refs(
        &self,
        frag: &bone_core::modules::FragmentScope,
        tree: &skull_parser::UseTree,
        prefix: &[Ustr],
        from_root: bool,
        out: &mut Vec<hir::PathSegmentRef>,
    ) {
        let skull_parser::UseTree::Path {
            from_root: tree_root,
            segments,
            kind,
        } = tree;
        let root = if prefix.is_empty() {
            *tree_root
        } else {
            from_root
        };
        let mut full = prefix.to_vec();
        full.extend(segments.iter().map(|s| s.name));
        let binds_item = matches!(
            kind,
            skull_parser::UseSuffix::Bind | skull_parser::UseSuffix::BindAs(_)
        );
        for (i, ident) in segments.iter().enumerate() {
            let upto = prefix.len() + i + 1;
            let target = if binds_item && upto == full.len() {
                self.use_leaf_target(frag, root, &full)
            } else {
                self.module_segment(frag, root, &full[..upto])
            };
            if let Some(target) = target {
                out.push(hir::PathSegmentRef {
                    span: ident.span,
                    target,
                    alias: false,
                });
            }
        }
        match kind {
            skull_parser::UseSuffix::BindAs(alias) => {
                if let Some(target) = self.use_leaf_target(frag, root, &full) {
                    out.push(hir::PathSegmentRef {
                        span: alias.span,
                        target,
                        alias: true,
                    });
                }
            }
            skull_parser::UseSuffix::Nested(children) => {
                for child in children {
                    self.use_tree_refs(frag, child, &full, root, out);
                }
            }
            skull_parser::UseSuffix::Operator { op, alias } => {
                full.push(op.name);
                if let Some(target) = self.use_leaf_target(frag, root, &full) {
                    out.push(hir::PathSegmentRef {
                        span: op.span,
                        target,
                        alias: false,
                    });
                    if let Some(alias) = alias {
                        out.push(hir::PathSegmentRef {
                            span: alias.span,
                            target,
                            alias: true,
                        });
                    }
                }
            }
            skull_parser::UseSuffix::Bind | skull_parser::UseSuffix::Glob => {}
        }
    }

    fn use_leaf_target(
        &self,
        frag: &bone_core::modules::FragmentScope,
        from_root: bool,
        segments: &[Ustr],
    ) -> Option<hir::SegmentTarget> {
        let path = bone_core::modules::UsePathRef {
            from_root,
            segments: segments.to_vec(),
        };
        match self.probed(|| resolve::resolve_path(self.workspace, frag, &path)) {
            Ok(resolved) => Some(hir::SegmentTarget::Def(resolved.key)),
            Err(_) => self.module_segment(frag, from_root, segments),
        }
    }

    fn resolve_bare_key(&self, name: Ustr, span: Span) -> Result<Ustr, CompilerError> {
        Ok(self.resolve_bare(name, span)?.key)
    }

    fn v1_path_callable(&self, path: &skull_parser::Path) -> Option<Ustr> {
        let frag = self.fragment_scope();
        let use_path = use_path_ref(path);
        match self.probed(|| resolve::resolve_member_path(self.workspace, frag, &use_path)) {
            Ok(resolve::MemberTarget::Callable(key)) => Some(key),
            Ok(resolve::MemberTarget::Global(key))
                if matches!(
                    self.workspace.defs.get(&key).map(|m| m.kind),
                    Some(bone_core::modules::DefKind::Callable)
                ) =>
            {
                Some(key)
            }
            // `p::NT(x)`. As a VALUE the path is the type, and the call then
            // lowers as a call of a value: inference reports the key as an
            // undefined global, because a newtype constructor is resolved by
            // name, exactly as `NT(x)` is inside the defining module.
            Ok(resolve::MemberTarget::Type(key))
                if self.workspace.nominals.newtype_id(key).is_some() =>
            {
                Some(key)
            }
            _ => None,
        }
    }

    fn v1_path_variant(&self, path: &skull_parser::Path) -> Option<(Ustr, Ustr)> {
        let frag = self.fragment_scope();
        let use_path = use_path_ref(path);
        match self.probed(|| resolve::resolve_member_path(self.workspace, frag, &use_path)) {
            Ok(resolve::MemberTarget::EnumVariant { enum_key, variant }) => {
                Some((enum_key, variant))
            }
            _ => None,
        }
    }

    // Stdlib names are already the final compiler-owned keys. Paths still
    // need their fragment scope, while bare names need no scoped lookup.
    fn name_scope(&self) -> Option<&bone_core::modules::FragmentScope> {
        let frag = self.fragment_scope();
        (frag.package != bone_core::modules::STD_PACKAGE_ID).then_some(frag)
    }

    fn resolve_bare(&self, name: Ustr, span: Span) -> Result<resolve::Resolved, CompilerError> {
        match self.name_scope() {
            Some(frag) => self.probed_cached(&self.resolutions.bare, name, || {
                resolve::resolve_bare(self.workspace, frag, name)
            }),
            None => Ok(resolve::plain(name)),
        }
        .map_err(|msg| CompilerError::new_with_label(span, msg))
    }

    fn resolve_callee_family(
        &mut self,
        name: Ustr,
        span: Span,
    ) -> Result<(Ustr, hir::CallFamily), CompilerError> {
        let resolved = match self.name_scope() {
            Some(frag) => self.probed_cached(&self.resolutions.callee, name, || {
                resolve::resolve_v1_callee(self.workspace, frag, name)
            }),
            None => Ok(resolve::CalleeFamily::Single(resolve::plain(name))),
        }
        .map_err(|msg| CompilerError::new_with_label(span, msg))?;
        Ok(self.record_family(name, resolved))
    }

    /// Resolve an optional unary-operator family; scalar operands may not use it.
    fn resolve_unary_family(
        &mut self,
        name: Ustr,
        span: Span,
    ) -> Result<hir::CallFamily, CompilerError> {
        Ok(self.resolve_optional_family(name, span)?.1)
    }

    /// Resolve the method family; a function-valued receiver field may win.
    fn resolve_method_family(
        &mut self,
        name: Ustr,
        span: Span,
    ) -> Result<(Ustr, hir::CallFamily), CompilerError> {
        self.resolve_optional_family(name, span)
    }

    fn resolve_optional_family(
        &mut self,
        name: Ustr,
        span: Span,
    ) -> Result<(Ustr, hir::CallFamily), CompilerError> {
        let resolved = match self.name_scope() {
            Some(frag) => self.probed_cached(&self.resolutions.optional, name, || {
                resolve::resolve_v1_optional_callee(self.workspace, frag, name)
            }),
            None => Ok(Some(resolve::CalleeFamily::Single(resolve::plain(name)))),
        }
        .map_err(|msg| CompilerError::new_with_label(span, msg))?;
        let resolved = resolved.unwrap_or(resolve::CalleeFamily::Merged {
            members: Vec::new(),
        });
        Ok(self.record_family(name, resolved))
    }

    fn capture_iterator_families(&mut self) {
        if self.module.iterator_families.is_some() {
            return;
        }
        let families = ["iter", "next"].map(|name| {
            let name = Ustr::from(name);
            let resolved = match self.name_scope() {
                Some(frag) => self.probed_cached(&self.resolutions.optional, name, || {
                    resolve::resolve_v1_optional_callee(self.workspace, frag, name)
                }),
                None => Ok(Some(resolve::CalleeFamily::Single(resolve::plain(name)))),
            };
            resolved.map(|family| {
                self.record_family(
                    name,
                    family.unwrap_or(resolve::CalleeFamily::Merged {
                        members: Vec::new(),
                    }),
                )
                .1
            })
        });
        self.module.iterator_families = Some(families);
    }

    fn record_family(
        &mut self,
        name: Ustr,
        resolved: resolve::CalleeFamily,
    ) -> (Ustr, hir::CallFamily) {
        let file = self.module.file;
        match resolved {
            resolve::CalleeFamily::Single(r) => {
                let spelling = if name.as_str().starts_with("__op_") {
                    name
                } else {
                    r.key
                };
                (spelling, hir::CallFamily::Resolved(r.key))
            }
            resolve::CalleeFamily::Merged { members } => {
                self.module.call_families.insert(name, members);
                (name, hir::CallFamily::Merged { name, file })
            }
        }
    }

    fn place_temp(&self, e: &ExprData) -> Option<Idx<BindingInfo>> {
        let key = e as *const ExprData as usize;
        self.place_temps
            .iter()
            .find(|(k, _)| *k == key)
            .map(|&(_, b)| b)
    }

    pub(crate) fn next_var_id(&mut self) -> VarId {
        self._next_var_id += 1;
        VarId(self._next_var_id)
    }

    pub(crate) fn alloc_binding(&mut self, b: BindingInfo, span: Span) -> Idx<BindingInfo> {
        let idx = self.body.binding_arena.alloc(b);

        self.body.binding_spans.insert(idx, span);
        self.node_interval(
            span.offset_range().0,
            span.offset_range().1,
            hir::NodeRef::Binding(idx),
        );

        idx
    }

    fn alloc_parameter(
        &mut self,
        name: &skull_parser::Ident,
        declared_ty: Option<TyId>,
        span: Span,
    ) -> Idx<BindingInfo> {
        self.alloc_binding(
            BindingInfo {
                name: name.name,
                kind: BindingKind::Param,
                declared_ty,
                span: name.span,
            },
            span,
        )
    }

    pub(crate) fn alloc_expr(&mut self, expr: Expr, span: Span) -> Idx<Expr> {
        let is_block = matches!(expr, Expr::Block { .. });
        let idx = self.body.expr_arena.alloc(expr);

        self.body.expr_spans.insert(idx, span);

        if !is_block {
            self.node_interval(
                span.offset_range().0,
                span.offset_range().1,
                hir::NodeRef::Expr(idx),
            );
        }

        idx
    }

    pub(crate) fn alloc_block_expr(&mut self, expr: Expr, b: &BlockData) -> Idx<Expr> {
        debug_assert!(matches!(expr, Expr::Block { .. }));
        let idx = self.body.expr_arena.alloc(expr);

        self.body.expr_spans.insert(idx, b.span);

        if let Some((open, close)) = b.braces {
            self.node_interval(
                open.offset_range().0,
                open.offset_range().1,
                hir::NodeRef::Expr(idx),
            );

            self.node_interval(
                close.offset_range().0,
                close.offset_range().1,
                hir::NodeRef::Expr(idx),
            );
        }

        idx
    }

    pub(crate) fn lower_type_exprs_to_range(
        &mut self,
        pos: TyPos,
        xs: &[Idx<TypeExprData>],
    ) -> Result<IdxRange<TypeExpr>, CompilerError> {
        let a = self.ast;
        let nodes: Vec<TypeExpr> = xs
            .iter()
            .map(|x| {
                let id = lower_type_expr_in(self, pos, &a.types[*x])?;
                Ok(self.type_node(id))
            })
            .collect::<Result<_, _>>()?;

        Ok(alloc_tys_contiguous(self, pos, nodes))
    }

    pub(crate) fn type_node(&self, ty: TyId) -> TypeExpr {
        match ty {
            TyId::Static(m, i) if m == self.module.module_id() => self.module.ty_arena[i],
            TyId::Static(m, i) => self.workspace.module(m).ty_arena[i],
            TyId::Local(i) => self.body.lowered_tys[i],
        }
    }

    pub(crate) fn declare_generics(&mut self, generics: &[skull_parser::Generic]) -> Vec<TyId> {
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

pub(crate) fn lower_type_expr(
    ctx: &mut LowerCtx,
    te: &skull_parser::TypeExprData,
) -> Result<TyId, CompilerError> {
    lower_type_expr_in(ctx, TyPos::Iface, te)
}

pub(crate) fn lower_type_expr_local(
    ctx: &mut LowerCtx,
    te: &skull_parser::TypeExprData,
) -> Result<TyId, CompilerError> {
    lower_type_expr_in(ctx, TyPos::Body, te)
}

/// Record spans only for new arena entries. Generics and `_` can share type IDs.
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
                && resolved_key.is_some_and(|key| ctx.workspace.nominals.contains_key(key))
        }
        _ => true,
    }
}

fn lower_type_application(
    ctx: &mut LowerCtx,
    pos: TyPos,
    te: &skull_parser::TypeExprData,
    key: Ustr,
) -> Result<TyId, CompilerError> {
    let a = ctx.ast;
    use skull_parser::TypeExprKind;

    let args = match &te.kind {
        TypeExprKind::App { args, .. } | TypeExprKind::AppPath { args, .. } => {
            Some(args.as_slice())
        }
        _ => None,
    };
    let is_field_ref = bone_core::modules::is_field_ref_former(key);
    if args.is_none() && !is_field_ref && !(ctx.workspace.nominals.contains_key(&key)) {
        let error = CompilerError::new_with_label(te.span, format!("Unknown type name {}", key));
        return match pos {
            TyPos::Iface => {
                ctx.module.interface_errors.push(error);
                Ok(ctx.workspace.builtins.error())
            }
            TyPos::Body => Err(error),
        };
    }

    if is_field_ref && args.is_some_and(|args| args.len() != 2) {
        let name = match &te.kind {
            TypeExprKind::App { ctor, .. } => ctor.name,
            _ => key,
        };
        comp_error!(
            te.span,
            "{} type must have exactly 2 type arguments (<Struct, FieldType>)",
            name
        );
    }
    let mut lowered = args
        .unwrap_or(&[])
        .iter()
        .map(|x| lower_type_expr_helper(ctx, pos, a.te(x)))
        .try_vec()?;

    // Exact intrinsic keys keep their own rules, even when a user type
    // has the same spelling in the workspace-wide generic map.
    let is_map = args.is_some() && bone_core::modules::is_intrinsic_key(key, "Map");
    if is_map && lowered.len() != 2 {
        comp_error!(te.span, "Map type must have exactly 2 type arguments");
    }
    if args.is_some() && bone_core::modules::is_intrinsic_key(key, "Coroutine") {
        while lowered.len() < 3 {
            lowered.push(TypeExpr::Builtin(hir::BuiltinTy::Unit));
        }
    }

    let entry = ctx.workspace.type_generics.get(&key);
    let arity = entry
        .map(|entry| entry.arity)
        .unwrap_or(if is_field_ref { 2 } else { 0 });
    let defaults = entry.map(|entry| entry.defaults.as_slice()).unwrap_or(&[]);
    let min = arity - defaults.len();
    let supplied = args.map_or(0, |args| args.len());
    let check_arity = args.is_none() || (entry.is_some() && !is_map && !is_field_ref);
    if check_arity && (supplied < min || supplied > arity) {
        let message = match (args, defaults.is_empty()) {
            (None, true) => format!("Generic type `{}` requires {} type argument(s)", key, arity),
            (None, false) => format!(
                "Generic type `{}` requires at least {} type argument(s)",
                key, min
            ),
            (Some(_), true) => format!(
                "Generic type `{}` expects {} type argument(s), found {}",
                key, arity, supplied
            ),
            (Some(_), false) => format!(
                "Generic type `{}` expects {} to {} type argument(s), found {}",
                key, min, arity, supplied
            ),
        };
        let error = CompilerError::new_with_label(te.span, message);
        if args.is_none() && matches!(pos, TyPos::Iface) {
            ctx.module.interface_errors.push(error);
            return Ok(ctx.workspace.builtins.error());
        }
        return Err(error);
    }
    if check_arity {
        while lowered.len() < arity {
            lowered.push(default_type_expr(defaults[lowered.len() - min]));
        }
    }

    let params = alloc_tys_contiguous(ctx, pos, lowered);
    let ctor = if args.is_some() {
        TypeCtor::for_name(key)
    } else {
        TypeCtor::Nominal(key)
    };
    Ok(alloc_ty(ctx, pos, TypeExpr::App { ctor, params }))
}

/// A type name that did not resolve, like an unknown type name: a signature
/// records it and types as `Error`, a body collects it with its other names.
fn unresolved_type_name(
    ctx: &mut LowerCtx,
    pos: TyPos,
    e: CompilerError,
) -> Result<TyId, CompilerError> {
    Ok(match pos {
        TyPos::Iface => {
            ctx.module.interface_errors.push(e);
            ctx.workspace.builtins.error()
        }
        TyPos::Body => {
            ctx.recover_name(e)?;
            alloc_ty(ctx, pos, TypeExpr::Builtin(hir::BuiltinTy::Error))
        }
    })
}

fn type_hole(ctx: &mut LowerCtx, pos: TyPos, span: Span, hint: Ustr) -> TyId {
    match pos {
        TyPos::Iface => {
            ctx.module
                .interface_errors
                .push(CompilerError::new_with_label(
                    span,
                    "a type hole is only allowed in a local annotation; write the declared type"
                        .to_string(),
                ));
            ctx.workspace.builtins.error()
        }
        TyPos::Body => {
            let var = ctx.next_var_id();
            alloc_ty(ctx, pos, TypeExpr::Infer(var, hint))
        }
    }
}

pub(crate) fn lower_type_expr_in(
    ctx: &mut LowerCtx,
    pos: TyPos,
    te: &skull_parser::TypeExprData,
) -> Result<TyId, CompilerError> {
    let a = ctx.ast;
    use skull_parser::TypeExprKind;

    let mut resolved_key = None;
    let idx = match &te.kind {
        TypeExprKind::Int => ctx.workspace.builtins.int(),
        TypeExprKind::Uint => ctx.workspace.builtins.uint(),
        TypeExprKind::Float | TypeExprKind::F64 => ctx.workspace.builtins.float(),
        TypeExprKind::F32 => {
            return Err(CompilerError::new_with_label(
                te.span,
                "`f32` is narrow float storage and is legal only as a struct field type; use `float`"
                    .to_string(),
            ));
        }
        TypeExprKind::Bool => ctx.workspace.builtins.bool(),
        TypeExprKind::String => ctx.workspace.builtins.string(),
        TypeExprKind::Unit => ctx.workspace.builtins.unit(),
        TypeExprKind::Any => ctx.workspace.builtins.any(),

        TypeExprKind::Path(path) | TypeExprKind::AppPath { ctor: path, .. } => {
            let frag = ctx.fragment_scope();
            let use_path = use_path_ref(path);
            let resolved =
                match ctx.probed(|| resolve::resolve_path(ctx.workspace, frag, &use_path)) {
                    Ok(r) => r,
                    Err(msg) => return Err(CompilerError::new_with_label(te.span, msg)),
                };
            ctx.record_path(pos, path, hir::SegmentTarget::Def(resolved.key));
            lower_type_application(ctx, pos, te, resolved.key)?
        }

        TypeExprKind::Ident(name) => {
            if name.name.as_str() == "_" {
                type_hole(ctx, pos, te.span, name.name)
            } else if let Some(g) = ctx.generics_in_scope.get(&name.name) {
                *g
            } else {
                match ctx.resolve_bare_key(name.name, name.span) {
                    Ok(bare_key) => {
                        resolved_key = Some(bare_key);
                        lower_type_application(ctx, pos, te, bare_key)?
                    }
                    Err(e) => unresolved_type_name(ctx, pos, e)?,
                }
            }
        }

        TypeExprKind::Tuple(xs) => {
            let elements = ctx.lower_type_exprs_to_range(pos, xs)?;
            alloc_ty(ctx, pos, hir::TypeExpr::Tuple { elements })
        }

        TypeExprKind::Array(inner) | TypeExprKind::Ref(inner) => {
            let ctor = match te.kind {
                TypeExprKind::Array(_) => TypeCtor::Array,
                _ => TypeCtor::Ref,
            };
            let inner = lower_type_expr_helper(ctx, pos, a.te(inner))?;
            let params = alloc_tys_contiguous(ctx, pos, [inner]);
            alloc_ty(ctx, pos, TypeExpr::App { ctor, params })
        }

        TypeExprKind::App { ctor, args } => 'app: {
            let resolved_ctor = match ctx.resolve_bare_key(ctor.name, ctor.span) {
                Ok(key) => key,
                // The arguments still lower, so a name among them is
                // reported in the same pass.
                Err(e) => {
                    let error = unresolved_type_name(ctx, pos, e)?;
                    for x in args.iter() {
                        lower_type_expr_helper(ctx, pos, a.te(x))?;
                    }
                    break 'app error;
                }
            };
            lower_type_application(ctx, pos, te, resolved_ctor)?
        }

        TypeExprKind::Function { params, ret } => {
            let lowered = params
                .iter()
                .map(|p| lower_type_expr_helper(ctx, pos, a.te(p)))
                .try_vec()?;

            let params_range = alloc_tys_contiguous(ctx, pos, lowered);

            let ret_te = lower_type_expr_helper(ctx, pos, a.te(ret))?;
            let ret_idx = alloc_ty_raw(ctx, pos, ret_te);

            alloc_ty(
                ctx,
                pos,
                TypeExpr::Function {
                    params: params_range,
                    ret: ret_idx,
                },
            )
        }

        TypeExprKind::Hole(_, name) => {
            type_hole(ctx, pos, te.span, name.unwrap_or_else(|| ustr("_")))
        }
    };

    let idx = if te.is_nullable {
        let inner_te = ctx.type_node(idx);
        let inner = alloc_ty_raw(ctx, pos, inner_te);

        alloc_ty(ctx, pos, TypeExpr::Nullable(inner))
    } else {
        idx
    };

    match idx {
        TyId::Static(mid, tidx) => {
            if mid != ctx.workspace.builtin_module().module_id() {
                if mid != ctx.module.module_id() {
                    comp_error!(
                        te.span,
                        "Lowered expr got a different module id than current module during lowering ... expected: {:?} ... actual {:?}",
                        ctx.module.module_id(),
                        mid
                    );
                } else if span_recordable(ctx, &te.kind, resolved_key.as_ref()) {
                    ctx.module.ty_spans.insert(tidx, te.span);
                }
            }

            ctx.node_interval(
                te.span.offset_range().0,
                te.span.offset_range().1,
                hir::NodeRef::StaticTy { mid, idx: tidx },
            );
        }

        TyId::Local(tidx) => {
            ctx.body.lowered_ty_spans.insert(tidx, te.span);
            ctx.node_interval(
                te.span.offset_range().0,
                te.span.offset_range().1,
                hir::NodeRef::LocalTy(tidx),
            );
        }
    }

    Ok(idx)
}

pub(crate) fn lookup_in_scope(
    ctx: &LowerCtx,
    start_scope: Idx<Scope>,
    name: &skull_parser::Ident,
) -> Result<Expr, CompilerError> {
    if let Some(binding) = lookup_local(&ctx.body, start_scope, name.name) {
        return Ok(Expr::Var(binding));
    }
    match ctx.resolve_bare_key(name.name, name.span) {
        Ok(key) => Ok(Expr::Global(key)),
        Err(e) => {
            ctx.recover_name(e)?;
            Ok(Expr::Error)
        }
    }
}

fn lookup_local(body: &Body, start_scope: Idx<Scope>, name: Ustr) -> Option<Idx<BindingInfo>> {
    let mut scope = start_scope;
    loop {
        if let Some(&binding) = body.scope_arena[scope].bindings.get(&name) {
            return Some(binding);
        }
        scope = body.scope_arena[scope].parent?;
    }
}

pub(crate) fn lower_block(
    ctx: &mut LowerCtx,
    parent: Idx<Scope>,
    block: &Idx<BlockData>,
) -> Result<Idx<Expr>, CompilerError> {
    let a = ctx.ast;
    let block = a.b(block);
    let value = lower_block_value(ctx, parent, block, None)?;
    Ok(ctx.alloc_block_expr(value, block))
}

fn lower_block_value(
    ctx: &mut LowerCtx,
    parent: Idx<Scope>,
    block: &BlockData,
    prelude: Option<(Statement, Span)>,
) -> Result<Expr, CompilerError> {
    let scope = ctx.body.alloc_child_scope(parent);

    let (statements, tail) = lower_statements(ctx, scope, &block.statements, prelude)?;

    Ok(Expr::Block { statements, tail })
}

pub(crate) fn lower_statement_value(
    ctx: &mut LowerCtx,
    scope: Idx<Scope>,
    s: &StatementData,
) -> Result<Statement, CompilerError> {
    let a = ctx.ast;
    let hir_s = match &s.kind {
        StatementKind::Use(u) => comp_error!(
            u.span,
            "`use` declarations require module resolution (not yet implemented)"
        ),
        StatementKind::Expression(expr) => Statement::Expr(lower_expr(ctx, scope, a.e(expr))?),

        StatementKind::Return {
            keyword_span,
            value: ret,
        } => {
            if let Some(expr) = &ret {
                let e_id = lower_expr(ctx, scope, a.e(expr))?;

                if let Some(kw_span) = keyword_span {
                    ctx.node_interval(
                        kw_span.offset_range().0,
                        kw_span.offset_range().1,
                        hir::NodeRef::Expr(e_id),
                    );
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
                lower_expr(ctx, scope, a.e(e))?
            } else {
                comp_error!(l.span, "let without value is not supported");
            };

            let declared_ty = if let Some(te) = &l.ty {
                Some(lower_type_expr_local(ctx, a.te(te))?)
            } else {
                None
            };

            // The else block runs on a pattern miss, so it must not see the
            // pattern's bindings: lower it before the pattern binds `scope`.
            let else_block = l
                .else_block
                .as_ref()
                .map(|b| lower_block(ctx, scope, b))
                .transpose()?;

            let pat = lower_pattern_value(ctx, scope, &l.pattern)?;
            let pattern = ctx.body.pattern_arena.alloc(pat);
            normalize_binding_wildcards(ctx, pattern, l.span);

            let mut value_idx = value_idx;
            if let Some(ty) = declared_ty {
                match ctx.body.pattern_single_bind(pattern) {
                    Some(b) => ctx.body.binding_arena[b].declared_ty = Some(ty),
                    // `let (a, b): (int, int) = e` — the annotation describes
                    // the whole value, so check it as an ascription.
                    None => {
                        value_idx = ctx.alloc_expr(
                            Expr::As {
                                value: value_idx,
                                target: ty,
                                kind: AsKind::Ascribe,
                            },
                            l.span,
                        )
                    }
                }
            }

            Statement::Let {
                pattern,
                value: value_idx,
                else_block,
            }
        }

        StatementKind::Static(st) => {
            reject_mutable_global_if_forbidden(a, st.name.span, st.name.name, "static")?;
            let value_idx = lower_expr(ctx, scope, a.e(&st.value))?;

            let declared_ty = if let Some(te) = &st.ty {
                Some(lower_type_expr_local(ctx, a.te(te))?)
            } else {
                None
            };

            let binding = BindingInfo {
                name: st.name.name,
                kind: BindingKind::Static,
                declared_ty,
                span: st.name.span,
            };

            let binding = ctx.alloc_binding(binding, st.name.span);
            ctx.body.scope_arena[scope]
                .bindings
                .insert(st.name.name, binding);

            Statement::Static {
                name: binding,
                value: value_idx,
            }
        }

        StatementKind::NestedFn(f) => {
            // Name binds in the ENCLOSING scope, BEFORE the body is lowered, so a
            // self-recursive call resolves; a not-yet-declared sibling stays a
            // Global (decl-before-use -> mutual recursion rejected in inference).
            let name_binding = ctx.alloc_binding(
                BindingInfo {
                    name: f.name.name,
                    kind: BindingKind::Fn,
                    declared_ty: None,
                    span: f.name.span,
                },
                f.name.span,
            );
            ctx.body.scope_arena[scope]
                .bindings
                .insert(f.name.name, name_binding);

            // Params live in a child scope whose parent is the enclosing scope,
            // so an enclosing param/local reference still resolves (then gets
            // rejected as a capture in inference), while the params shadow.
            let fn_scope = ctx.body.alloc_child_scope(scope);
            let first_param = ctx.body.binding_arena.len();
            for p in &f.params {
                let ty = lower_type_expr_local(ctx, a.te(&p.ty))?;
                let b = ctx.alloc_parameter(&p.name, Some(ty), p.span);
                ctx.body.scope_arena[fn_scope]
                    .bindings
                    .insert(p.name.name, b);
            }
            let params = ctx.body.binding_arena.allocated_since(first_param);

            let ret_ty = match &f.return_ty {
                Some(te) => Some(lower_type_expr_local(ctx, a.te(te))?),
                None => None,
            };

            let saved = ctx.placeholder_forbidden;
            ctx.placeholder_forbidden = true;
            let body_idx = lower_block(ctx, fn_scope, &f.body)?;
            ctx.placeholder_forbidden = saved;

            Statement::Fn {
                name: name_binding,
                params,
                ret_ty,
                body: body_idx,
            }
        }

        StatementKind::For(f) => {
            let iter = lower_expr(ctx, scope, a.e(&f.iter))?;

            let loop_scope = ctx.body.alloc_child_scope(scope);

            let pat = lower_pattern_value(ctx, loop_scope, &f.pattern)?;
            let pattern = ctx.body.pattern_arena.alloc(pat);
            let names_only = normalize_binding_wildcards(ctx, pattern, f.span);

            if let Some(te) = &f.ty {
                let ty = lower_type_expr_local(ctx, a.te(te))?;
                match ctx.body.pattern_single_bind(pattern) {
                    Some(b) => ctx.body.binding_arena[b].declared_ty = Some(ty),
                    None => comp_error!(
                        f.span,
                        "a `for` element type annotation requires a plain name binding"
                    ),
                }
            }

            let (pattern, prelude) = if names_only {
                (pattern, None)
            } else {
                // Loop codegen wants a plain element binding; anything richer
                // destructures through a `let` at the body's head.
                let elem = ctx.alloc_binding(
                    BindingInfo {
                        name: ustr::ustr("__for_elem"),
                        kind: hir::BindingKind::Let,
                        declared_ty: None,
                        span: f.span,
                    },
                    f.span,
                );
                let elem_pat = ctx.body.pattern_arena.alloc(hir::Pattern::Bind(elem));
                let elem_var = ctx.alloc_expr(Expr::Var(elem), f.span);
                let destructure = Statement::Let {
                    pattern,
                    value: elem_var,
                    else_block: None,
                };
                (elem_pat, Some((destructure, f.span)))
            };
            let block = a.b(&f.body);
            let value = lower_block_value(ctx, loop_scope, block, prelude)?;
            Statement::For {
                pattern,
                iter,
                body: ctx.alloc_block_expr(value, block),
            }
        }

        StatementKind::While(w) => {
            let body_i = lower_block(ctx, scope, &w.body)?;

            Statement::While {
                cond: lower_expr(ctx, scope, a.e(&w.condition))?,
                body: body_i,
            }
        }

        // `while let P = v { .. }` IS `while true { let P = v else { break } .. }`.
        StatementKind::WhileLet(w) => {
            let body_scope = ctx.body.alloc_child_scope(scope);

            let value = lower_expr(ctx, body_scope, a.e(&w.value))?;
            let pat = lower_pattern_value(ctx, body_scope, &w.pattern)?;
            let pattern = ctx.body.pattern_arena.alloc(pat);
            normalize_binding_wildcards(ctx, pattern, w.span);

            let brk = ctx.body.statement_arena.alloc(Statement::Break);
            ctx.body.statement_spans.insert(brk, w.span);
            let else_block = ctx.alloc_expr(
                Expr::Block {
                    statements: IdxRange::new_inclusive(brk..=brk),
                    tail: None,
                },
                w.span,
            );
            let test = Statement::Let {
                pattern,
                value,
                else_block: Some(else_block),
            };

            let block = a.b(&w.body);
            let body_expr = lower_block_value(ctx, body_scope, block, Some((test, w.span)))?;
            let body_i = ctx.alloc_block_expr(body_expr, block);
            let cond = ctx.alloc_expr(Expr::Bool(true), w.span);

            Statement::While { cond, body: body_i }
        }

        StatementKind::Defer(dk) => {
            let deferred = match dk {
                DeferKind::Block(b) => lower_block(ctx, scope, b)?,

                DeferKind::Expression(e) => lower_expr(ctx, scope, a.e(e))?,
            };

            Statement::Defer(deferred)
        }

        StatementKind::LifecycleHook {
            keyword_span,
            name,
            body: hook_body,
        } => {
            let name = name.as_str();
            let block = a.b(hook_body);

            if name == "on_killed" || name == "on_resume" || name == "on_hot_reload" {
                let hook_scope = ctx.body.alloc_child_scope(scope);
                let body_idx = lower_block(ctx, hook_scope, hook_body)?;

                let lambda = ctx.alloc_expr(
                    Expr::Lambda {
                        params: IdxRange::new_empty(),
                        body: body_idx,
                    },
                    block.span,
                );

                let args = IdxRange::new_inclusive(lambda..=lambda);
                let name_span = ctx.body.name_spans.alloc(keyword_span.unwrap_or(s.span));
                let call = ctx.alloc_expr(
                    Expr::Call {
                        callee: Callee::Unresolved {
                            name: ustr(name),
                            name_span,
                        },
                        args,
                        generic_args: IdxRange::new_empty(),
                        family: hir::CallFamily::Resolved(ustr(name)),
                    },
                    s.span,
                );

                Statement::Expr(call)
            } else {
                let e = lower_block_value(ctx, scope, block, None)?;
                Statement::Expr(ctx.alloc_expr(e, block.span))
            }
        }
    };

    Ok(hir_s)
}

pub(crate) fn lower_expr_value(
    ctx: &mut LowerCtx,
    scope: Idx<Scope>,
    e: &ExprData,
) -> Result<Expr, CompilerError> {
    let a = ctx.ast;
    use hir::Expr;
    use skull_parser::ExprKind;

    if let Some(binding) = ctx.place_temp(e) {
        return Ok(Expr::Var(binding));
    }

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
            return lower_placeholder_lambda(ctx, scope, e);
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
                let file =
                    match e.span.file == ctx.module.file && !ctx.module.logical_file.is_empty() {
                        true => ctx.module.logical_file,
                        false => ctx.workspace.logical_file(e.span.file),
                    };
                Expr::String(ustr(&format!("{}:{}", file, e.span.line_range().0 + 1)))
            } else if name.name.as_str() == "_" && ctx.placeholder_param.is_some() {
                if ctx.placeholder_forbidden {
                    comp_error!(
                        e.span,
                        "`_` placeholder cannot appear inside an explicit lambda or block expression"
                    );
                }

                Expr::Var(ctx.placeholder_param.unwrap())
            } else {
                lookup_in_scope(ctx, scope, name)?
            }
        }

        ExprKind::InterpolatedString(parts) => {
            Expr::InterpolatedString(lower_interp_parts(ctx, scope, parts)?)
        }

        ExprKind::Block(block) => {
            let saved = ctx.placeholder_forbidden;

            ctx.placeholder_forbidden = true;
            let r = lower_block_value(ctx, scope, a.b(block), None);
            ctx.placeholder_forbidden = saved;

            return r;
        }

        ExprKind::FunctionCall {
            func,
            generic_args,
            args,
            syntax: _,
        } => {
            let generic_args = ctx.lower_type_exprs_to_range(TyPos::Body, generic_args)?;
            match &a.e(func).kind {
                ExprKind::Ident(name) => {
                    let args_range = lower_exprs(ctx, scope, args.iter().map(|x| &x.value))?;

                    for (arg_idx, ca) in args_range.into_iter().zip(args.iter()) {
                        if let Some(nm) = &ca.name {
                            ctx.body.arg_names.insert(arg_idx, nm.name);
                        }
                        if ca.is_spread {
                            ctx.body.spread_args.insert(arg_idx);
                        }
                    }

                    lower_named_call(ctx, scope, name, args_range, generic_args)?
                }

                ExprKind::FieldAccess(receiver, method) => {
                    if !generic_args.is_empty() {
                        comp_error!(
                            e.span,
                            "explicit type arguments are only supported on named function calls"
                        );
                    }
                    let all = std::iter::once(receiver).chain(args.iter().map(|a| &a.value));

                    let args_range = lower_exprs(ctx, scope, all)?;

                    for (arg_idx, ca) in args_range.into_iter().skip(1).zip(args.iter()) {
                        if let Some(nm) = &ca.name {
                            ctx.body.arg_names.insert(arg_idx, nm.name);
                        }
                        if ca.is_spread {
                            ctx.body.spread_args.insert(arg_idx);
                        }
                    }

                    ctx.body.method_call_recvs.insert(args_range.start());
                    ctx.body
                        .method_names
                        .insert(args_range.start(), method.name);

                    let (callee_key, family) =
                        ctx.resolve_method_family(method.name, method.span)?;
                    Expr::Call {
                        callee: Callee::Unresolved {
                            name: callee_key,
                            name_span: ctx.body.name_spans.alloc(method.span),
                        },
                        args: args_range,
                        generic_args: IdxRange::new_empty(),
                        family,
                    }
                }

                // `a::b::f(..)` in a v1 fragment. Without this the path lowers as a
                // VALUE (`Expr::Global`) and the call becomes `Callee::Value`, which
                // MIR cannot give a callee identity -- a direct call must carry the
                // family's DefKey exactly as the bare-name form does.
                ExprKind::Path(path) if ctx.v1_path_callable(path).is_some() => {
                    let key = ctx.v1_path_callable(path).expect("guard resolved it");
                    ctx.record_path(TyPos::Body, path, hir::SegmentTarget::Def(key));
                    let args_range = lower_exprs(ctx, scope, args.iter().map(|x| &x.value))?;
                    for (arg_idx, ca) in args_range.into_iter().zip(args.iter()) {
                        if let Some(nm) = &ca.name {
                            ctx.body.arg_names.insert(arg_idx, nm.name);
                        }
                    }
                    Expr::Call {
                        callee: hir::Callee::Unresolved {
                            name: key,
                            name_span: ctx
                                .body
                                .name_spans
                                .alloc(path.segments.last().map_or(e.span, |segment| segment.span)),
                        },
                        args: args_range,
                        generic_args,
                        family: hir::CallFamily::Resolved(key),
                    }
                }

                // A path enum constructor with payload; value-position paths lower
                // as unit variants and would otherwise drop these arguments.
                ExprKind::Path(path) if ctx.v1_path_variant(path).is_some() => {
                    let (enum_key, variant) = ctx.v1_path_variant(path).expect("guard resolved it");
                    let (enum_span, variant_span) = variant_path_spans(path);
                    ctx.record_path(
                        TyPos::Body,
                        path,
                        hir::SegmentTarget::Variant { enum_key, variant },
                    );
                    if !generic_args.is_empty() {
                        comp_error!(
                            e.span,
                            "explicit type arguments are not supported on enum constructors"
                        );
                    }
                    let args_range = lower_exprs(ctx, scope, args.iter().map(|x| &x.value))?;
                    Expr::EnumVariant {
                        enum_name: enum_key,
                        enum_name_span: ctx.body.name_spans.alloc(enum_span),
                        variant,
                        variant_span: ctx.body.name_spans.alloc(variant_span),
                        args: VariantArgs::Tuple(args_range),
                    }
                }

                ExprKind::SafeNavigation(receiver, method) => {
                    let receiver = lower_expr(ctx, scope, a.e(receiver))?;
                    let args_range = lower_exprs(ctx, scope, args.iter().map(|a| &a.value))?;

                    for (arg_idx, ca) in args_range.into_iter().zip(args.iter()) {
                        if let Some(nm) = &ca.name {
                            ctx.body.arg_names.insert(arg_idx, nm.name);
                        }
                    }

                    ctx.body.method_names.insert(receiver, method.name);
                    let (callee_key, family) =
                        ctx.resolve_method_family(method.name, method.span)?;
                    Expr::SafeNavCall {
                        receiver,
                        method: callee_key,
                        args: args_range,
                        family,
                    }
                }

                _ => {
                    if !generic_args.is_empty() {
                        comp_error!(
                            e.span,
                            "explicit type arguments require a named function callee"
                        );
                    }
                    let callee = lower_expr(ctx, scope, a.e(func))?;
                    let args_range = lower_exprs(ctx, scope, args.iter().map(|a| &a.value))?;

                    Expr::Call {
                        callee: hir::Callee::Value(callee),
                        args: args_range,
                        generic_args: IdxRange::new_empty(),
                        // The callee is a value, so inference calls its type and
                        // never asks the family for a name; the placeholder key is
                        // not a resolvable spelling.
                        family: hir::CallFamily::Resolved(ustr("<value>")),
                    }
                }
            }
        }

        ExprKind::ArrayLiteral(elems) => {
            let elems = lower_exprs(ctx, scope, elems.iter())?;
            Expr::Array(elems)
        }

        ExprKind::If {
            branches,
            else_branch,
        } => {
            let mut acc = else_branch
                .as_ref()
                .map(|b| lower_block(ctx, scope, b))
                .transpose()?;

            for (cond, block) in branches[1..].iter().rev() {
                let cond = lower_expr(ctx, scope, a.e(cond))?;
                let then = lower_block(ctx, scope, block)?;

                let idx = ctx.alloc_expr(
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
            let cond = lower_expr(ctx, scope, a.e(cond))?;
            let then = lower_block(ctx, scope, block)?;

            Expr::If {
                cond,
                then_branch: then,
                else_branch: acc,
            }
        }

        ExprKind::Tuple(items) => Expr::Tuple(lower_exprs(ctx, scope, items.iter())?),

        ExprKind::BinOp {
            left, op, right, ..
        } if *op == skull_parser::TokenKind::DoubleDot => Expr::Range {
            low: lower_expr(ctx, scope, a.e(left))?,
            high: lower_expr(ctx, scope, a.e(right))?,
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

            if !lvalue_has_safe_nav(a, left) {
                let temps = compound_place_temps(a, left);
                if !temps.is_empty() {
                    return lower_compound_assign_with_temps(
                        ctx, scope, e, op_fn, left, right, &temps,
                    );
                }
            }

            let target = lower_expr(ctx, scope, a.e(left))?;
            let (callee_key, family) = ctx.resolve_callee_family(ustr(op_fn), e.span)?;

            if lvalue_has_safe_nav(a, left) {
                let value = lower_expr(ctx, scope, a.e(right))?;
                Expr::CompoundAssign {
                    target,
                    op_fn: ustr(op_fn),
                    value,
                    family,
                }
            } else {
                let args = lower_exprs(ctx, scope, [left, right].into_iter())?;

                let name_span = ctx.body.name_spans.alloc(e.span);
                let call = ctx.alloc_expr(
                    Expr::Call {
                        callee: Callee::Unresolved {
                            name: callee_key,
                            name_span,
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
            left,
            op,
            op_span,
            right,
        } => {
            if let Some(op_fn) = op.overloadable_binop_fn() {
                let args = lower_exprs(ctx, scope, [left, right].into_iter())?;
                return lower_named_call(
                    ctx,
                    scope,
                    &skull_parser::Ident {
                        name: ustr(op_fn),
                        span: *op_span,
                    },
                    args,
                    IdxRange::new_empty(),
                );
            }
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
                lhs: lower_expr(ctx, scope, a.e(left))?,
                rhs: lower_expr(ctx, scope, a.e(right))?,
            }
        }

        ExprKind::FieldAccess(expr, field) => match &a.e(expr).kind {
            ExprKind::Ident(base) if base.name.as_str() == "_G" => {
                match ctx.resolve_bare_key(field.name, field.span) {
                    Ok(key) => Expr::Global(key),
                    Err(e) => {
                        ctx.recover_name(e)?;
                        Expr::Error
                    }
                }
            }
            _ => {
                let base = lower_expr(ctx, scope, a.e(expr))?;

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

        ExprKind::Path(path) => {
            let frag = ctx.fragment_scope();
            let use_path = use_path_ref(path);
            let target =
                ctx.probed(|| resolve::resolve_member_path(ctx.workspace, frag, &use_path));
            if let Ok(t) = &target {
                ctx.record_path(TyPos::Body, path, member_segment_target(t));
            }
            match target {
                Ok(resolve::MemberTarget::Global(key) | resolve::MemberTarget::Callable(key)) => {
                    Expr::Global(key)
                }
                Ok(resolve::MemberTarget::EnumVariant { enum_key, variant }) => {
                    let (enum_span, variant_span) = variant_path_spans(path);
                    Expr::EnumVariant {
                        enum_name: enum_key,
                        enum_name_span: ctx.body.name_spans.alloc(enum_span),
                        variant,
                        variant_span: ctx.body.name_spans.alloc(variant_span),
                        args: hir::VariantArgs::Unit,
                    }
                }
                Ok(resolve::MemberTarget::Type(key)) => Expr::Global(key),
                Err(msg) => {
                    ctx.recover_name(CompilerError::new_with_label(e.span, msg))?;
                    Expr::Error
                }
            }
        }

        ExprKind::PathStructLiteral {
            path,
            bases,
            fields,
        } => {
            let unqualified = path.segments.len() == 1 && !path.from_root;
            let base_range = if unqualified {
                lower_exprs(ctx, scope, bases.iter())?
            } else {
                IdxRange::new_empty()
            };
            let frag = ctx.fragment_scope();
            let use_path = use_path_ref(path);
            let target =
                match ctx.probed(|| resolve::resolve_member_path(ctx.workspace, frag, &use_path)) {
                    Ok(t) => t,
                    Err(msg) => {
                        ctx.recover_name(CompilerError::new_with_label(e.span, msg))?;
                        for (_, value) in fields.iter() {
                            lower_expr(ctx, scope, a.e(value))?;
                        }
                        return Ok(Expr::Error);
                    }
                };
            ctx.record_path(TyPos::Body, path, member_segment_target(&target));
            if !unqualified && !bases.is_empty() {
                return Err(CompilerError::new_with_label(
                    e.span,
                    "functional-update `..base` is not supported on qualified record literals"
                        .to_string(),
                ));
            }
            let mut entries = Vec::new();
            for (names, value) in fields.iter() {
                let v = lower_expr(ctx, scope, a.e(value))?;
                for fname in names {
                    entries.push(StructLitField {
                        name: fname.name,
                        name_span: fname.span,
                        value: v,
                    });
                }
            }
            let fields_range = alloc_contiguous(&mut ctx.body.struct_lit_field_arena, entries);
            match target {
                resolve::MemberTarget::Type(key) => Expr::StructLiteral {
                    name: key,
                    fields: fields_range,
                    bases: base_range,
                },
                resolve::MemberTarget::EnumVariant { enum_key, variant } => {
                    let (enum_span, variant_span) = variant_path_spans(path);
                    Expr::EnumVariant {
                        enum_name: enum_key,
                        enum_name_span: ctx.body.name_spans.alloc(enum_span),
                        variant,
                        variant_span: ctx.body.name_spans.alloc(variant_span),
                        args: hir::VariantArgs::Record(fields_range),
                    }
                }
                resolve::MemberTarget::Global(key) | resolve::MemberTarget::Callable(key) => {
                    return Err(CompilerError::new_with_label(
                        e.span,
                        format!("`{key}` names a value, not a record"),
                    ));
                }
            }
        }

        ExprKind::SiblingFieldRef(ident) => Expr::SiblingRef(ident.name),

        ExprKind::Assign { target, value } => {
            let target = lower_expr(ctx, scope, a.e(target))?;
            let value = lower_expr(ctx, scope, a.e(value))?;
            Expr::Assign { target, value }
        }

        ExprKind::Match { scrutinee, arms } => {
            let scrutinee = lower_expr(ctx, scope, a.e(scrutinee))?;
            let mut arm_vals = Vec::with_capacity(arms.len());

            for arm in arms.iter() {
                let arm_scope = ctx.body.alloc_child_scope(scope);

                let pat = lower_pattern_value(ctx, arm_scope, &arm.pattern)?;
                let pattern = ctx.body.pattern_arena.alloc(pat);

                let body_i = lower_block(ctx, arm_scope, &arm.body)?;

                let guard = match &arm.guard {
                    Some(g) => Some(lower_expr(ctx, arm_scope, a.e(g))?),
                    None => None,
                };

                arm_vals.push(MatchArm {
                    pattern,
                    body: body_i,
                    guard,
                });
            }

            let arms = alloc_contiguous(&mut ctx.body.match_arm_arena, arm_vals);
            Expr::Match { scrutinee, arms }
        }

        ExprKind::Paren(inner) => lower_expr_value(ctx, scope, a.e(inner))?,
        ExprKind::Not(inner) => {
            let family = ctx.resolve_unary_family(ustr("__not"), e.span)?;
            Expr::Not(lower_expr(ctx, scope, a.e(inner))?, family)
        }
        ExprKind::BitNot(inner) => Expr::BitNot(lower_expr(ctx, scope, a.e(inner))?),

        ExprKind::As(inner, ty) => Expr::As {
            value: lower_expr(ctx, scope, a.e(inner))?,
            target: lower_type_expr_local(ctx, a.te(ty))?,
            kind: AsKind::Cast,
        },

        ExprKind::Bitcast(inner, ty) => Expr::As {
            value: lower_expr(ctx, scope, a.e(inner))?,
            target: lower_type_expr_local(ctx, a.te(ty))?,
            kind: AsKind::Bitcast,
        },

        ExprKind::Index(base, index) => Expr::Index {
            base: lower_expr(ctx, scope, a.e(base))?,
            index: lower_expr(ctx, scope, a.e(index))?,
        },

        ExprKind::SafeNavigation(base, field) => Expr::SafeNav {
            base: lower_expr(ctx, scope, a.e(base))?,
            field: field.name,
        },

        ExprKind::Lambda {
            params,
            body: body_e,
            ..
        } => {
            let lambda_scope = ctx.body.alloc_child_scope(scope);

            let first_param = ctx.body.binding_arena.len();
            for p in params {
                let declared_ty =
                    p.ty.map(|te| lower_type_expr_local(ctx, a.te(&te)))
                        .transpose()?;
                let b = ctx.alloc_parameter(&p.name, declared_ty, p.span);
                ctx.body.scope_arena[lambda_scope]
                    .bindings
                    .insert(p.name.name, b);
            }
            let params_range = ctx.body.binding_arena.allocated_since(first_param);

            let saved = ctx.placeholder_forbidden;
            ctx.placeholder_forbidden = true;
            let body_idx = lower_block(ctx, lambda_scope, body_e)?;
            ctx.placeholder_forbidden = saved;

            Expr::Lambda {
                params: params_range,
                body: body_idx,
            }
        }

        ExprKind::UnaryMinus(inner) => {
            let family = ctx.resolve_unary_family(ustr("__unm"), e.span)?;
            Expr::UnaryMinus(lower_expr(ctx, scope, a.e(inner))?, family)
        }

        ExprKind::Yield(opt) => {
            let inner = match opt {
                Some(e) => Some(lower_expr(ctx, scope, a.e(e))?),
                None => None,
            };

            Expr::Yield(inner)
        }

        ExprKind::YieldDelegate(inner) => Expr::YieldDelegate(lower_expr(ctx, scope, a.e(inner))?),

        ExprKind::IfLetChain {
            clauses,
            then_block,
            else_block,
        } => {
            let idx = lower_if_let_clauses(ctx, scope, clauses, then_block, else_block)?;
            ctx.body.expr_arena[idx]
        }

        ExprKind::Spread(inner) => Expr::Spread(lower_expr(ctx, scope, a.e(inner))?),

        ExprKind::SafeNavigationLet {
            receiver,
            binding,
            body: block,
        } => {
            let receiver = lower_expr(ctx, scope, a.e(receiver))?;

            let body_scope = ctx.body.alloc_child_scope(scope);

            let b = ctx.alloc_binding(
                BindingInfo {
                    name: binding.name,
                    kind: BindingKind::Let,
                    declared_ty: None,
                    span: binding.span,
                },
                binding.span,
            );

            ctx.body.scope_arena[body_scope]
                .bindings
                .insert(binding.name, b);

            Expr::SafeNavLet {
                receiver,
                binding: b,
                body: lower_block(ctx, body_scope, block)?,
            }
        }

        ExprKind::ParenAscription(inner, ty) => Expr::As {
            value: lower_expr(ctx, scope, a.e(inner))?,
            target: lower_type_expr_local(ctx, a.te(ty))?,
            kind: AsKind::Ascribe,
        },

        ExprKind::PreIncrement(inner) => Expr::IncDec {
            target: lower_expr(ctx, scope, a.e(inner))?,
            decrement: false,
            postfix: false,
        },

        ExprKind::PostIncrement(inner) => Expr::IncDec {
            target: lower_expr(ctx, scope, a.e(inner))?,
            decrement: false,
            postfix: true,
        },

        ExprKind::PreDecrement(inner) => Expr::IncDec {
            target: lower_expr(ctx, scope, a.e(inner))?,
            decrement: true,
            postfix: false,
        },

        ExprKind::PostDecrement(inner) => Expr::IncDec {
            target: lower_expr(ctx, scope, a.e(inner))?,
            decrement: true,
            postfix: true,
        },

        ExprKind::ReturnExpr(inner) => Expr::Return(match inner {
            Some(e) => Some(lower_expr(ctx, scope, a.e(e))?),
            None => None,
        }),

        ExprKind::BreakExpr => Expr::Break,
        ExprKind::ContinueExpr => Expr::Continue,

        ExprKind::FunctionRef(ident) => match ctx.resolve_bare_key(ident.name, ident.span) {
            Ok(key) => Expr::Global(key),
            Err(e) => {
                ctx.recover_name(e)?;
                Expr::Error
            }
        },

        ExprKind::AnaphoricIdent(name) => lookup_in_scope(ctx, scope, name)?,

        ExprKind::Hole(_, name) => Expr::Hole(*name),

        ExprKind::QuestionMark(..) => {
            comp_error!(e.span, "unsupported expr {:?}", e.kind)
        }
    };

    Ok(result)
}

pub(crate) fn lower_expr(
    ctx: &mut LowerCtx,
    scope: Idx<Scope>,
    e: &skull_parser::ExprData,
) -> Result<Idx<hir::Expr>, CompilerError> {
    ctx.capture_iterator_families();
    if let ExprKind::Block(b) = &e.kind {
        if let Some(binding) = ctx.place_temp(e) {
            return Ok(ctx.alloc_expr(Expr::Var(binding), e.span));
        }
        let saved = ctx.placeholder_forbidden;
        ctx.placeholder_forbidden = true;
        let r = lower_block(ctx, scope, b);
        ctx.placeholder_forbidden = saved;

        return r;
    }

    let hir_e = lower_expr_value(ctx, scope, e)?;
    Ok(ctx.alloc_expr(hir_e, e.span))
}

fn function_footprint_lo(annotations: &[Annotation], f: &FunctionDef) -> usize {
    annotations
        .iter()
        .map(|a| a.span.offset_range().0)
        .min()
        .unwrap_or(f.span.offset_range().0)
}

/// Lower a fn's generic bounds: `I: Iterator<T>` becomes `Some(elem)` at I's
/// index. Runs after `declare_generics` so a bound may reference a later
/// sibling generic (`<I: Iterator<T>, T>`).
fn lower_generic_bounds(
    ctx: &mut LowerCtx,
    generics: &[skull_parser::Generic],
) -> Result<Vec<Option<TyId>>, CompilerError> {
    let a = ctx.ast;
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
                out.push(Some(lower_type_expr(ctx, a.te(&args[0]))?));
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

/// Validate defaults even when their lowered forms are unused here.
fn type_def_generics(
    ctx: &mut LowerCtx,
    what: &str,
    generics: &[skull_parser::Generic],
) -> Result<Vec<TyId>, CompilerError> {
    let a = ctx.ast;
    reject_generic_bounds(what, generics)?;
    validate_generic_defaults(what, generics, a)?;
    let ids = ctx.declare_generics(generics);
    ctx.body = Body::default();
    for g in generics {
        if let Some(d) = &g.default {
            lower_type_expr(ctx, a.te(d))?;
        }
    }
    Ok(ids)
}

fn insert_item_body(ctx: &mut LowerCtx, owner: BodyOwner, span: Span) {
    let body = ctx.take_body();
    ctx.module.insert_body(owner, body);
    let (start, stop) = span.offset_range();
    ctx.module.make_lapper(start, stop, owner);
}

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
                K::Int | K::Uint | K::Float | K::F64 | K::Bool | K::String | K::Unit | K::Any => {
                    true
                }
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

pub(crate) fn build_function(
    ctx: &mut LowerCtx,
    f: &FunctionDef,
    annotations: &[Annotation],
) -> Result<(Function, Option<Body>, Vec<CompilerError>), CompilerError> {
    let a = ctx.ast;
    ctx.generics_in_scope.clear();
    reject_generic_defaults("fn generics", &f.generics)?;
    reject_inline_lua_if_forbidden(a, f)?;
    let generic_var_ids = ctx.declare_generics(&f.generics);
    ctx.body = Body::default();
    let generic_bounds = lower_generic_bounds(ctx, &f.generics)?;

    let return_ty = if let Some(ret_ty) = &f.return_ty {
        let typ_expr = lower_type_expr(ctx, a.te(ret_ty))?;
        Some(typ_expr)
    } else {
        None
    };

    let mut param_decls = Vec::with_capacity(f.params.len());
    let mut param_tys = Vec::with_capacity(f.params.len());

    for p in f.params.iter() {
        let ty_ref = lower_type_expr(ctx, a.te(&p.ty))?;
        param_tys.push(ty_ref);
        param_decls.push(ParamDecl {
            name: p.name.name,
            ty: ty_ref,
            has_default: p.default.is_some(),
            annotations: lower_annotations_with_ctx(ctx, &p.annotations)?,
        });
    }

    // Registry violations accumulate as lowering errors (the recovery channel)
    // rather than failing fast: the workspace must stay buildable while an
    // annotation is mid-typed (decorator completion runs on it), and
    // `body_errors()` already turns any lowering error into a hard verdict.
    let annotations = lower_annotations_with_ctx(ctx, annotations)?;
    let anno_errors = fn_annotation_errors(ctx, &annotations, &param_decls);

    let (body_failed, mut errors) = lower_fn_body_into(ctx, f, &param_tys)?;
    use skull_parser::FunctionBody;
    let (inline_body, is_extern, ffi_lib, backend) = match f.body {
        FunctionBody::Rock(_) => (None, false, None, hir::FnBackend::Any),
        FunctionBody::Inline(backend, source) => (Some((backend, source)), false, None, backend),
        FunctionBody::ExternLua => (None, true, None, hir::FnBackend::Lua),
        FunctionBody::ExternJs => (None, true, None, hir::FnBackend::Js),
        FunctionBody::Ffi(lib) => (None, true, Some(lib), hir::FnBackend::Lua),
    };
    errors.extend(anno_errors);
    if let Err(e) = bone_core::annotations::validate_param_repr_against_body(
        f.name.name,
        &param_decls,
        inline_body.is_some() || is_extern,
    ) {
        errors.push(e);
    }

    let is_variadic = f.params.last().map_or(false, |p| p.is_variadic);
    let func = hir::Function {
        span: f.span,
        name: f.name.name,
        name_span: f.name.span,

        is_coroutine: f.is_coroutine,
        is_variadic,
        generics: generic_var_ids,
        generic_bounds,
        param_decls,

        num_required: required_params(&f.params),
        return_ty,
        inline_body,
        is_extern,
        backend,
        annotations,
        ffi_lib,
        is_synthetic: f.is_synthetic,
        json_field_for: f
            .json_field_for
            .as_ref()
            .map(|(owner, field, operation)| {
                ctx.resolve_bare_key(owner.name, owner.span)
                    .map(|key| (key, *field, *operation))
            })
            .transpose()?,
    };

    let body = ctx.take_body();
    Ok((func, (!body_failed).then_some(body), errors))
}

fn fn_annotation_errors(
    ctx: &LowerCtx,
    annotations: &[hir::Annotation],
    params: &[ParamDecl],
) -> Vec<CompilerError> {
    let names = &ctx.workspace.decorator_names;
    std::iter::once(validate_annotations(AnnoPos::FnItem, annotations, names))
        .chain(
            params
                .iter()
                .map(|pd| validate_annotations(AnnoPos::Param, &pd.annotations, names)),
        )
        .filter_map(Result::err)
        .collect()
}

/// Install a re-lowered body and REPLACE its lowering errors, clearing stale
/// ones from a prior edit. Returns whether a body was installed.
fn install_relowered_body(
    module: &mut hir::Module,
    owner: BodyOwner,
    body: Option<Body>,
    errors: Vec<CompilerError>,
) -> bool {
    if errors.is_empty() {
        module.lowering_errors.remove(&owner);
    } else {
        module.lowering_errors.insert(owner, errors);
    }
    match body {
        Some(body) => {
            module.insert_body(owner, body);
            true
        }
        None => {
            module.bodies.remove(&owner);
            false
        }
    }
}

/// Lower the body using resolved interface types. Body-only refreshes must
/// avoid appending replacements to the module type arena.
fn lower_param_defaults(
    ctx: &mut LowerCtx,
    scope: Idx<Scope>,
    params: &[skull_parser::Param],
) -> Result<Vec<Option<Idx<Expr>>>, CompilerError> {
    let a = ctx.ast;
    let mut defaults = Vec::with_capacity(params.len());
    for param in params {
        defaults.push(match &param.default {
            Some(expr) => Some(lower_expr(ctx, scope, a.e(expr))?),
            None => None,
        });
    }
    Ok(defaults)
}

fn required_params(params: &[skull_parser::Param]) -> usize {
    params
        .iter()
        .filter(|p| p.default.is_none() && !p.is_variadic)
        .count()
}

fn lower_fn_body_into(
    ctx: &mut LowerCtx,
    f: &FunctionDef,
    param_tys: &[TyId],
) -> Result<(bool, Vec<CompilerError>), CompilerError> {
    debug_assert_eq!(
        f.params.len(),
        param_tys.len(),
        "lower_fn_body_into needs one resolved param ty per AST param"
    );

    // Returned rather than stored so a body re-lower replaces its errors.
    let mut bindings = FxHashMap::default();
    let default_scope = ctx.body.alloc_root_scope();

    let first_param = ctx.body.binding_arena.len();
    for (p, &ty) in f.params.iter().zip(param_tys) {
        let binding = ctx.alloc_parameter(&p.name, Some(ty), p.span);
        bindings.insert(p.name.name, binding);
    }
    let range = ctx.body.binding_arena.allocated_since(first_param);

    // Defaults are lowered after every param binding exists: a lambda default
    // allocates its own param bindings, which would otherwise land inside the
    // `first..=last` param range below. An unresolved name in a default fails
    // the body.
    let (defaults, mut errors) =
        ctx.collect_name_errors(|ctx| lower_param_defaults(ctx, default_scope, &f.params));
    let param_defaults = defaults?;

    let root_scope = ctx.body.alloc_root_scope_with_bindings(bindings);
    let mut body_failed = false;
    if let skull_parser::FunctionBody::Rock(block) = &f.body {
        let (lowered, names) = ctx.collect_name_errors(|ctx| lower_block(ctx, root_scope, block));
        errors.extend(names);
        match lowered {
            Ok(root) => ctx.body.root = Some(root),
            Err(error) => {
                errors.push(error);
                body_failed = true;
            }
        }
    }
    ctx.body.root_scope = Some(root_scope);
    ctx.body.param_bindings = range;
    ctx.body.param_defaults = param_defaults;

    Ok((body_failed, errors))
}

pub fn lower_function_def(
    ctx: &mut LowerCtx,
    f: &FunctionDef,
    annotations: &[Annotation],
    item_lo: usize,
    reg_key: Ustr,
) -> Result<Idx<Function>, CompilerError> {
    let (mut func, body, errors) = build_function(ctx, f, annotations)?;
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
            f.span.offset_range().1,
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
    ws.relowered_body_count
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Reuse the signature types so body edits do not grow the module arena.
    let sig_reusable = module.functions[fid].param_decls.len() == f.params.len()
        && module.functions[fid].generics.len() == f.generics.len();

    if !sig_reusable {
        // The signature changed after all: rebuild the whole function, which may
        // grow the module arena.
        let high_water = module.lower_var_high_water;
        let mut ctx = LowerCtx::new(ws, module, a, high_water);

        let (func, body, errors) = build_function(&mut ctx, f, annotations)?;
        ctx.flush_scope_probes();

        ctx.module.lower_var_high_water = ctx._next_var_id;
        ctx.module.functions[fid] = func;
        return Ok(install_relowered_body(ctx.module, owner, body, errors));
    }

    // Reuse the resolved signature: `lower_fn_body_into` allocates only into the
    // fresh `Body`, never the module arena.
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
        generics_in_scope,
        ..LowerCtx::new(ws, module, a, high_water)
    };

    // Rebuild the signature's navigation spans with body-local type IDs.
    if let Some(ret_ty) = &f.return_ty {
        lower_type_expr_local(&mut ctx, a.te(ret_ty))?;
    }
    for p in f.params.iter() {
        lower_type_expr_local(&mut ctx, a.te(&p.ty))?;
    }

    let (body_failed, mut errors) = lower_fn_body_into(&mut ctx, f, &param_tys)?;
    ctx.module.lower_var_high_water = ctx._next_var_id;

    // Re-validate annotations on the body-only path too: the REPLACE below
    // would otherwise clear a still-present annotation violation recorded by
    // the last full lower.
    let lowered_annos = lower_annotations_with_ctx(&ctx, annotations)?;
    errors.extend(fn_annotation_errors(
        &ctx,
        &lowered_annos,
        &ctx.module.functions[fid].param_decls,
    ));
    ctx.flush_scope_probes();

    // The signature is kept as-is EXCEPT its span: `shift_module_after_edit`
    // deliberately skips the edited fn's own span (it expected `build_function`
    // to replace the whole `Function`), so patch the new full-item span +
    // name span here -- otherwise `body_at` / navigation go stale on the next edit.
    let existing = &mut ctx.module.functions[fid];
    existing.span = f.span;
    existing.name_span = f.name.span;

    let body = (!body_failed).then(|| ctx.take_body());
    Ok(install_relowered_body(ctx.module, owner, body, errors))
}

// Runtime marker keys: eq, inspect, deepcopy, and call dispatch duck-type on
// their presence, and `__rock_create_struct` overwrites `__ty` -- a user field
// with one of these names silently misbehaves, so reject at the declaration.
fn is_reserved_runtime_field(name: &str) -> bool {
    matches!(name, "__fn" | "__ty" | "__variant")
}

pub fn lower_struct_def(
    ctx: &mut LowerCtx,
    s: &skull_parser::StructDef,
    reg_key: Ustr,
) -> Result<(), CompilerError> {
    let generic_var_ids = type_def_generics(ctx, "a struct", &s.generics)?;

    let mut fields = Vec::new();
    let def_scope = ctx.body.alloc_root_scope();
    // Like a function body, the defaults report every name they cannot
    // resolve; they then keep no value, so inference never sees a placeholder.
    let (lowered, names) =
        ctx.collect_name_errors(|ctx| lower_struct_fields(ctx, s, def_scope, &mut fields));
    lowered?;

    let idx = ctx.module.nominals.struct_id(reg_key).unwrap().def;
    ctx.module.structs[idx].fields = fields;
    ctx.module.structs[idx].generics = generic_var_ids;

    if !names.is_empty() {
        ctx.body.field_defaults.clear();
        ctx.module
            .lowering_errors
            .insert(BodyOwner::StructDefaults(idx), names);
    }
    insert_item_body(ctx, BodyOwner::StructDefaults(idx), s.span);

    Ok(())
}

fn lower_struct_fields(
    ctx: &mut LowerCtx,
    s: &skull_parser::StructDef,
    def_scope: Idx<Scope>,
    fields: &mut Vec<hir::StructFieldDef>,
) -> Result<(), CompilerError> {
    let a = ctx.ast;
    let mut json_names = FxHashSet::default();
    for field in s.fields.iter() {
        let (ty, storage_ty) = match a.te(&field.ty).kind {
            skull_parser::TypeExprKind::F32 => {
                (ctx.workspace.builtins.float(), ctx.workspace.builtins.f32())
            }
            _ => {
                let ty = lower_type_expr(ctx, a.te(&field.ty))?;
                (ty, ty)
            }
        };
        let default = field
            .default
            .as_ref()
            .map(|e| lower_expr(ctx, def_scope, a.e(e)))
            .transpose()?;

        if fields
            .iter()
            .any(|prev: &hir::StructFieldDef| prev.name == field.name.name)
        {
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

        let read_vis = match &field.vis {
            Some(v) => bone_core::modules::NsVisibility::from_parser(v),
            None => bone_core::modules::NsVisibility::Pub,
        };
        let write_vis = if field.set_private {
            bone_core::modules::NsVisibility::Private
        } else {
            read_vis.clone()
        };
        if field.set_private
            && !bone_core::modules::is_setter_audience_within_getter(&write_vis, &read_vis)
        {
            comp_error!(
                field.name.span,
                "write audience must be within read audience: field `{}` has `private(set)` but getter is already private",
                field.name.name
            );
        }
        let field_annotations = lower_annotations_with_ctx(ctx, &field.annotations)?;
        validate_annotations(
            AnnoPos::StructField,
            &field_annotations,
            &ctx.workspace.decorator_names,
        )?;
        let json_default = field_annotations.iter().any(|a| {
            a.is_builtin("json_default")
                || a.is_builtin("json_skip")
                || a.is_builtin("json_omit_if")
        });
        if json_default && field.default.is_none() {
            comp_error!(
                field.name.span,
                "JSON default and skipped fields require a declared default"
            );
        }
        if json_default && !s.generics.is_empty() {
            comp_error!(
                field.name.span,
                "JSON default factories for generic records are not supported yet"
            );
        }
        let wire_name = json_field_name(&field_annotations).unwrap_or(field.name.name);
        if !json_names.insert(wire_name) {
            comp_error!(
                field.name.span,
                "duplicate JSON field name `{}` in struct `{}`",
                wire_name,
                s.name.name
            );
        }
        fields.push(hir::StructFieldDef {
            name: field.name.name,
            ty,
            storage_ty,
            span: field.name.span,
            has_default: field.default.is_some(),
            annotations: field_annotations,
            is_embed: field.is_embed,
            read_vis,
            write_vis,
        });

        if let Some(default) = default {
            ctx.body.field_defaults.insert(field.name.name, default);
        }
    }
    Ok(())
}

pub fn lower_enum_def(
    ctx: &mut LowerCtx,
    e: &skull_parser::EnumDef,
    reg_key: Ustr,
) -> Result<(), CompilerError> {
    let a = ctx.ast;
    let generic_var_ids = type_def_generics(ctx, "an enum", &e.generics)?;

    let mut variants = Vec::new();

    for v in e.variants.iter() {
        use skull_parser::EnumVariantKind;

        let payload = match &v.kind {
            EnumVariantKind::Unit => VariantPayload::Unit,
            EnumVariantKind::Tuple(fs) => {
                VariantPayload::Tuple(fs.iter().map(|f| lower_type_expr(ctx, a.te(f))).try_vec()?)
            }
            EnumVariantKind::Record(fs) => {
                let rec: Vec<hir::StructFieldDef> = fs
                    .iter()
                    .map(|f| {
                        let annos = lower_annotations_with_ctx(ctx, &f.annotations)?;
                        validate_annotations(AnnoPos::EnumVariantField, &annos, &ctx.workspace.decorator_names)?;
                        if f.is_embed {
                            comp_error!(
                                f.name.span,
                                "`embed` is not allowed on an enum variant field `{}`; embed inlines leaves into a struct owner",
                                f.name.name
                            );
                        }
                        let ty = lower_type_expr(ctx, a.te(&f.ty))?;
                        Ok(hir::StructFieldDef {
                            name: f.name.name,
                            ty,
                            storage_ty: ty,
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

        if variants
            .iter()
            .any(|prev: &EnumVariantDef| prev.name == v.name.name)
        {
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

    let idx = ctx.module.nominals.enum_id(reg_key).unwrap().def;
    ctx.module.enums[idx].variants = variants;
    ctx.module.enums[idx].generics = generic_var_ids;

    insert_item_body(ctx, BodyOwner::Enum(idx), e.span);

    Ok(())
}

pub fn lower_newtype_def(
    ctx: &mut LowerCtx,
    n: &skull_parser::NewtypeDef,
    reg_key: Ustr,
) -> Result<(), CompilerError> {
    let a = ctx.ast;
    let generic_var_ids = type_def_generics(ctx, "a newtype", &n.generics)?;

    let idx = ctx.module.nominals.newtype_id(reg_key).unwrap().def;

    ctx.module.newtypes[idx].wrapped = lower_type_expr(ctx, a.te(&n.wrapped))?;
    ctx.module.newtypes[idx].generics = generic_var_ids;

    insert_item_body(ctx, BodyOwner::Newtype(idx), n.span);

    Ok(())
}

fn lower_type_expr_helper(
    ctx: &mut LowerCtx,
    pos: TyPos,
    orig_te: &skull_parser::TypeExprData,
) -> Result<hir::TypeExpr, CompilerError> {
    let id = lower_type_expr_in(ctx, pos, orig_te)?;
    Ok(ctx.type_node(id))
}

fn lower_exprs<'a>(
    ctx: &mut LowerCtx,
    scope: Idx<Scope>,
    exprs: impl Iterator<Item = &'a Idx<ExprData>>,
) -> Result<IdxRange<Expr>, CompilerError> {
    let a = ctx.ast;
    let mut lowered_args = Vec::new();

    for e in exprs {
        let e = a.e(e);
        lowered_args.push((lower_expr_value(ctx, scope, e)?, e));
    }

    let start = ctx.body.expr_arena.len();
    for (value, expr) in lowered_args {
        ctx.alloc_expr(value, expr.span);
    }
    Ok(ctx.body.expr_arena.allocated_since(start))
}

fn lower_statements(
    ctx: &mut LowerCtx,
    scope: Idx<Scope>,
    statements: &[Idx<StatementData>],
    prelude: Option<(Statement, Span)>,
) -> Result<(IdxRange<Statement>, Option<Idx<Expr>>), CompilerError> {
    let a = ctx.ast;
    let mut lowered = Vec::new();
    if let Some((s, span)) = prelude {
        lowered.push((s, span));
    }
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
                tail = Some((lower_expr_value(ctx, scope, e)?, e));
            }
        } else {
            lowered.push((lower_statement_value(ctx, scope, s)?, s.span));
        }
    }

    let tail = tail.map(|(expr, data)| ctx.alloc_expr(expr, data.span));
    let start = ctx.body.statement_arena.len();
    for (statement, span) in lowered {
        ctx.body.alloc_statement(statement, span);
    }
    Ok((ctx.body.statement_arena.allocated_since(start), tail))
}

pub fn lower_global(
    ctx: &mut LowerCtx,
    g: &skull_parser::GlobalDef,
    reg_key: Ustr,
) -> Result<(), CompilerError> {
    let a = ctx.ast;
    if !g.is_const {
        reject_mutable_global_if_forbidden(a, g.name.span, g.name.name, "global")?;
    }
    let name = reg_key;
    ctx.body = Body::default();

    let ty = match &g.ty {
        Some(te) => lower_type_expr(ctx, a.te(te))?,
        None => comp_error!(
            g.span,
            "Global variable {} must have an explicit type annotation",
            name
        ),
    };

    // Like a function body, the initializer reports every name it cannot
    // resolve; it then keeps no value, so inference never sees a placeholder.
    let (value, names) = match &g.value {
        Some(e) => {
            let scope = ctx.body.alloc_root_scope();
            let (lowered, mut names) =
                ctx.collect_name_errors(|ctx| lower_expr(ctx, scope, a.e(e)));
            let value = match lowered {
                Ok(lowered) if names.is_empty() => Some(lowered),
                Ok(_) => None,
                Err(e) if names.is_empty() => return Err(e),
                Err(e) => {
                    names.push(e);
                    None
                }
            };
            (value, names)
        }
        None => (None, Vec::new()),
    };

    if !names.is_empty() {
        ctx.body = Body::default();
    }
    ctx.body.root = value;

    let idx = ctx.module.globals.alloc(GlobalDef {
        name,
        name_span: g.name.span,
        ty,
        span: g.span,
        is_const: g.is_const,
    });

    if ctx.module.global_by_name.insert(name, idx).is_some() {
        comp_error!(g.span, "Duplicate global variable definition: `{}`", name);
    }

    if !names.is_empty() {
        ctx.module
            .lowering_errors
            .insert(BodyOwner::Global(idx), names);
    }
    insert_item_body(ctx, BodyOwner::Global(idx), g.span);

    Ok(())
}

/// A `globals = "none"` package may still use immutable `const` declarations,
/// but mutable package-level storage would make its explicit simulation root
/// incomplete. The owning file supplies this fact; no declaration overrides it.
fn reject_mutable_global_if_forbidden(
    a: &Ast,
    span: Span,
    name: ustr::Ustr,
    kind: &str,
) -> Result<(), CompilerError> {
    if !matches!(a.global_policy, skull_parser::GlobalPolicy::None) {
        return Ok(());
    }
    let package = a
        .package
        .as_ref()
        .map(|tag| tag.self_name.as_str())
        .unwrap_or("<standalone>");
    comp_error!(
        span,
        "mutable {kind} `{name}` is not allowed in package `{package}` with [language] globals = \"none\"; declare immutable state as `const`"
    );
}

/// A `globals = "none"` package cannot hide writes in a native Lua body: the
/// state checker has no expression arena to derive its write set from.
fn reject_inline_lua_if_forbidden(a: &Ast, f: &FunctionDef) -> Result<(), CompilerError> {
    if f.is_synthetic
        || !matches!(a.global_policy, skull_parser::GlobalPolicy::None)
        || !matches!(
            &f.body,
            skull_parser::FunctionBody::Inline(skull_parser::FnBackend::Lua, _)
        )
    {
        return Ok(());
    }
    let package = a
        .package
        .as_ref()
        .map(|tag| tag.self_name.as_str())
        .unwrap_or("<standalone>");
    comp_error!(
        f.name.span,
        "inline(lua) function `{}` is not allowed in package `{}` with [language] globals = \"none\"; move the host operation to a package with an explicit native-state boundary",
        f.name.name,
        package
    );
}

/// Resolve and record a modules-v1 qualified variant pattern's path to the
/// enum's DefKey and variant. On a name error the sub-patterns still lower and
/// `None` tells the caller to lower a wildcard.
fn resolve_pattern_path<'p>(
    ctx: &mut LowerCtx,
    scope: Idx<Scope>,
    path: &skull_parser::Path,
    subpats: impl IntoIterator<Item = &'p skull_parser::Pattern>,
) -> Result<Option<(Ustr, Ustr)>, CompilerError> {
    let frag = ctx.fragment_scope();
    let use_path = use_path_ref(path);
    match ctx.probed(|| resolve::resolve_variant_pattern(ctx.workspace, frag, &use_path)) {
        Ok((enum_key, variant)) => {
            ctx.record_path(
                TyPos::Body,
                path,
                hir::SegmentTarget::Variant { enum_key, variant },
            );
            Ok(Some((enum_key, variant)))
        }
        Err(msg) => {
            ctx.recover_name(CompilerError::new_with_label(path.span(), msg))?;
            for p in subpats {
                lower_pattern_value(ctx, scope, p)?;
            }
            Ok(None)
        }
    }
}

/// Normalize `_` holes for let/for binding and report names-only loop shapes.
/// Or-patterns stay untouched because their holes cannot bind.
fn normalize_binding_wildcards(ctx: &mut LowerCtx, p: Idx<Pattern>, span: Span) -> bool {
    let pattern = ctx.body.pattern_arena[p];
    let mut names_only = matches!(
        pattern,
        Pattern::Wildcard | Pattern::Bind(_) | Pattern::Tuple { .. }
    );
    match pattern {
        Pattern::Wildcard => {
            let b = ctx.alloc_binding(
                BindingInfo {
                    name: ustr::ustr("_"),
                    kind: hir::BindingKind::Let,
                    declared_ty: None,
                    span,
                },
                span,
            );
            ctx.body.pattern_arena[p] = Pattern::Bind(b);
        }
        Pattern::Newtype { sub, .. } => {
            normalize_binding_wildcards(ctx, sub, span);
        }
        Pattern::Record { fields, .. } => {
            for f in fields {
                let sub = ctx.body.record_pat_field_arena[f].pat;
                normalize_binding_wildcards(ctx, sub, span);
            }
        }
        Pattern::EnumVariant { sub, .. } | Pattern::Tuple { elements: sub } => {
            for e in sub {
                names_only &= normalize_binding_wildcards(ctx, e, span);
            }
        }
        Pattern::Bind(_) | Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Or { .. } => {}
    }
    names_only
}

fn lower_pattern_value(
    ctx: &mut LowerCtx,
    scope: Idx<Scope>,
    pat: &skull_parser::Pattern,
) -> Result<Pattern, CompilerError> {
    let a = ctx.ast;
    Ok(match pat {
        skull_parser::Pattern::Wildcard => Pattern::Wildcard,
        skull_parser::Pattern::Ident(id) => {
            let b = ctx.alloc_binding(
                BindingInfo {
                    name: id.name,
                    kind: hir::BindingKind::Let,
                    declared_ty: None,
                    span: id.span,
                },
                id.span,
            );

            ctx.body.scope_arena[scope].bindings.insert(id.name, b);

            Pattern::Bind(b)
        }
        skull_parser::Pattern::PathVariant { path, args } => {
            // A newtype name takes precedence over shorthand enum inference.
            if path.segments.len() == 1
                && !path.from_root
                && let Some(subs) = args
                && let segment = &path.segments[0]
                && let Ok(key) = ctx.resolve_bare_key(segment.name, segment.span)
                && ctx.workspace.nominals.newtype_id(key).is_some()
            {
                if subs.len() != 1 {
                    comp_error!(
                        segment.span,
                        "newtype pattern `{}` takes exactly one argument",
                        segment.name
                    );
                }
                let sub = lower_pattern_value(ctx, scope, &subs[0])?;
                let sub = ctx.body.pattern_arena.alloc(sub);
                return Ok(Pattern::Newtype {
                    name: key,
                    name_span: segment.span,
                    sub,
                });
            }
            // A single unrooted segment preserves shorthand enum inference;
            // longer paths resolve the enum and variant through the package.
            let (enum_name, enum_name_span, variant, variant_span) = if path.segments.len() == 1
                && !path.from_root
            {
                let segment = &path.segments[0];
                (None, None, segment.name, segment.span)
            } else {
                let subpats = args.iter().flatten();
                let Some((enum_key, variant)) = resolve_pattern_path(ctx, scope, path, subpats)?
                else {
                    return Ok(Pattern::Wildcard);
                };
                let (enum_span, variant_span) = variant_path_spans(path);
                (Some(enum_key), Some(enum_span), variant, variant_span)
            };

            let sub = match args {
                None => IdxRange::new_empty(),
                Some(pats) => {
                    let vals = pats
                        .iter()
                        .map(|p| lower_pattern_value(ctx, scope, p))
                        .try_vec()?;
                    alloc_contiguous(&mut ctx.body.pattern_arena, vals)
                }
            };
            Pattern::EnumVariant {
                enum_name,
                enum_name_span,
                variant,
                variant_span,
                sub,
            }
        }
        skull_parser::Pattern::PathRecord { path, fields, rest } => {
            // An unqualified record name still resolves as a bare type name,
            // falling back to the written variant name when it names no struct.
            let (enum_name, enum_name_span, name, name_span) = if path.segments.len() == 1
                && !path.from_root
            {
                let segment = &path.segments[0];
                let name = ctx
                    .resolve_bare_key(segment.name, segment.span)
                    .ok()
                    .filter(|key| ctx.workspace.nominals.struct_id(*key).is_some())
                    .unwrap_or(segment.name);
                (None, None, Some(name), Some(segment.span))
            } else {
                let subpats = fields.iter().map(|(_, p)| p);
                let Some((enum_key, variant)) = resolve_pattern_path(ctx, scope, path, subpats)?
                else {
                    return Ok(Pattern::Wildcard);
                };
                let (enum_span, variant_span) = variant_path_spans(path);
                (
                    Some(enum_key),
                    Some(enum_span),
                    Some(variant),
                    Some(variant_span),
                )
            };
            let mut entries = Vec::with_capacity(fields.len());
            for (fname, subpat) in fields.iter() {
                let pat = lower_pattern_value(ctx, scope, subpat)?;
                let pat = ctx.body.pattern_arena.alloc(pat);
                entries.push(RecordPatField {
                    name: fname.name,
                    name_span: fname.span,
                    pat,
                });
            }
            let fields = alloc_contiguous(&mut ctx.body.record_pat_field_arena, entries);
            Pattern::Record {
                enum_name,
                enum_name_span,
                name,
                name_span,
                fields,
                rest: *rest,
            }
        }
        skull_parser::Pattern::Range {
            start,
            end,
            inclusive,
        } => {
            let start = start
                .as_ref()
                .map(|e| lower_expr(ctx, scope, a.e(e)))
                .transpose()?;
            let end = end
                .as_ref()
                .map(|e| lower_expr(ctx, scope, a.e(e)))
                .transpose()?;

            Pattern::Range {
                start,
                end,
                inclusive: *inclusive,
            }
        }

        skull_parser::Pattern::Literal(expr) => {
            let e = lower_expr(ctx, scope, a.e(expr))?;
            Pattern::Literal(e)
        }
        skull_parser::Pattern::Tuple(elems) => {
            let vals = elems
                .iter()
                .map(|p| lower_pattern_value(ctx, scope, p))
                .try_vec()?;

            let elements = alloc_contiguous(&mut ctx.body.pattern_arena, vals);

            Pattern::Tuple { elements }
        }

        skull_parser::Pattern::Or(alts) => {
            let vals = alts
                .iter()
                .map(|p| lower_pattern_value(ctx, scope, p))
                .try_vec()?;

            let alternatives = alloc_contiguous(&mut ctx.body.pattern_arena, vals);

            Pattern::Or { alternatives }
        }
    })
}

fn lower_if_let_clauses(
    ctx: &mut LowerCtx,
    scope: Idx<Scope>,
    clauses: &[IfLetClause],
    then_block: &Idx<BlockData>,
    else_block: &Option<Idx<BlockData>>,
) -> Result<Idx<Expr>, CompilerError> {
    // The else block is lowered ONCE and its Idx shared by every clause's miss
    // arm: each clause failing lands on the same HIR, so its diagnostics and
    // bindings exist once instead of once per clause.
    let else_branch = match else_block {
        Some(blk) => {
            let e = lower_block(ctx, scope, blk)?;
            // Every clause's miss arm lands here, so no clause's branch-local
            // narrowing may be applied to it.
            if clauses.len() > 1 {
                ctx.body.shared_branch_targets.insert(e);
            }
            Some(e)
        }
        None => None,
    };
    lower_if_let_clauses_with(ctx, scope, clauses, then_block, else_branch)
}

fn lower_if_let_clauses_with(
    ctx: &mut LowerCtx,
    scope: Idx<Scope>,
    clauses: &[IfLetClause],
    then_block: &Idx<BlockData>,
    else_branch: Option<Idx<Expr>>,
) -> Result<Idx<Expr>, CompilerError> {
    let a = ctx.ast;
    let Some((clause, rest)) = clauses.split_first() else {
        return lower_block(ctx, scope, then_block);
    };

    let node = match clause {
        IfLetClause::Binding { pattern, value } => {
            let value = lower_expr(ctx, scope, a.e(value))?;
            let then_scope = ctx.body.alloc_child_scope(scope);

            let pat = lower_pattern_value(ctx, then_scope, pattern)?;

            let pattern = ctx.body.pattern_arena.alloc(pat);
            let then_branch =
                lower_if_let_clauses_with(ctx, then_scope, rest, then_block, else_branch)?;

            Expr::IfLet {
                pattern,
                value,
                then_branch,
                else_branch,
            }
        }

        IfLetClause::Condition(cond) => {
            let cond = lower_expr(ctx, scope, a.e(cond))?;
            let then_branch = lower_if_let_clauses_with(ctx, scope, rest, then_block, else_branch)?;

            Expr::If {
                cond,
                then_branch,
                else_branch,
            }
        }
    };

    Ok(ctx.alloc_expr(node, a.b(then_block).span))
}

fn lower_interp_parts(
    ctx: &mut LowerCtx,
    scope: Idx<Scope>,
    parts: &[skull_parser::InterpolatedPart],
) -> Result<IdxRange<InterpPart>, CompilerError> {
    let a = ctx.ast;
    use skull_parser::InterpolatedPart as P;
    // Lower every part before allocating any: a hole can hold its own
    // interpolated string, whose parts would otherwise land inside this range.
    let mut lowered = Vec::with_capacity(parts.len());

    for part in parts {
        let hir = match part {
            P::Literal(s) => InterpPart::Literal(*s),

            P::Expr(e, spec) => {
                let e = a.e(e);
                let val = lower_expr_value(ctx, scope, e)?;
                let expr = ctx.alloc_expr(val, e.span);

                let spec = match spec {
                    Some(s) => {
                        let width_expr = match s.width_expr {
                            Some(we) => {
                                let we = a.e(&we);
                                let v = lower_expr_value(ctx, scope, we)?;
                                Some(ctx.alloc_expr(v, we.span))
                            }

                            None => None,
                        };

                        let precision_expr = match s.precision_expr {
                            Some(pe) => {
                                let pe = a.e(&pe);
                                let v = lower_expr_value(ctx, scope, pe)?;
                                Some(ctx.alloc_expr(v, pe.span))
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

        lowered.push(hir);
    }

    Ok(alloc_contiguous(&mut ctx.body.interp_part_arena, lowered))
}

pub fn lower_decorator_def(
    ctx: &mut LowerCtx,
    d: &skull_parser::DecoratorDef,
    annotations: &[skull_parser::Annotation],
    def_key: Ustr,
) -> Result<Idx<DecoratorDef>, CompilerError> {
    let a = ctx.ast;
    ctx.generics_in_scope.clear();
    ctx.body = Body::default();

    let first_param = ctx.body.binding_arena.len();
    let mut param_decls = Vec::new();
    let mut bindings = FxHashMap::default();

    for p in &d.params {
        let ty_ref = lower_type_expr(ctx, a.te(&p.ty))?;
        let binding = ctx.alloc_parameter(&p.name, Some(ty_ref), p.span);
        bindings.insert(p.name.name, binding);
        let deco_param_annotations = lower_annotations_with_ctx(ctx, &p.annotations)?;
        validate_annotations(
            AnnoPos::DecoParam,
            &deco_param_annotations,
            &ctx.workspace.decorator_names,
        )?;
        param_decls.push(ParamDecl {
            name: p.name.name,
            ty: ty_ref,
            has_default: p.default.is_some(),
            annotations: deco_param_annotations,
        });
    }

    let params_range = ctx.body.binding_arena.allocated_since(first_param);
    let target_ty = {
        let idx = ctx.module.ty_arena.alloc(TypeExpr::App {
            ctor: TypeCtor::Nominal(ustr("__DecoTarget")),
            params: IdxRange::new_empty(),
        });

        TyId::Static(ctx.module.module_id(), idx)
    };

    let target = ctx.alloc_binding(
        BindingInfo {
            name: ustr("target"),
            kind: BindingKind::Param,
            declared_ty: Some(target_ty),
            span: d.name.span,
        },
        d.span,
    );

    bindings.insert(ustr("target"), target);

    let root_scope = ctx.body.alloc_root_scope_with_bindings(bindings);

    // Like a function body, the defaults and body report every name they
    // cannot resolve; the body is then dropped, so inference never sees a
    // placeholder.
    let (lowered, names) = ctx.collect_name_errors(|ctx| {
        let defaults = lower_param_defaults(ctx, root_scope, &d.params)?;
        let root = lower_block(ctx, root_scope, &d.body)?;
        Ok((defaults, root))
    });
    let (param_defaults, body_e) = lowered?;

    ctx.body.root = Some(body_e);
    ctx.body.root_scope = Some(root_scope);
    ctx.body.param_bindings = params_range;
    ctx.body.param_defaults = param_defaults;
    ctx.body.deco_target = Some(target);

    let idx = ctx.module.decorators.alloc(DecoratorDef {
        name: d.name.name,
        def_key,
        span: d.span,
        param_decls,
        num_required: required_params(&d.params),
        annotations: lower_annotations_with_ctx(ctx, annotations)?,
    });

    if !names.is_empty() {
        ctx.module
            .lowering_errors
            .insert(BodyOwner::Decorator(idx), names);
        return Ok(idx);
    }
    insert_item_body(ctx, BodyOwner::Decorator(idx), d.span);

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

/// Lower annotation syntax and resolve v1 decorator names through the
/// current fragment.
/// Compiler-owned metadata intentionally stays unresolved so its raw spelling
/// remains the source of builtin behavior.
pub fn lower_annotations_with_ctx(
    ctx: &LowerCtx,
    anns: &[skull_parser::Annotation],
) -> Result<Vec<hir::Annotation>, CompilerError> {
    lower_annotations_in_scope(ctx, anns, ctx.fragment_scope())
}

/// Lower annotations using the logical module path of their declaration.
pub fn lower_annotations_in_scope(
    ctx: &LowerCtx,
    anns: &[skull_parser::Annotation],
    scope: &bone_core::modules::FragmentScope,
) -> Result<Vec<hir::Annotation>, CompilerError> {
    let mut lowered = lower_annotations(ctx.ast, anns);
    for anno in &mut lowered {
        // Only a decorator claims an annotation spelling; a same-named fn or
        // global leaves the builtin in force.
        let resolved = ctx
            .probed(|| resolve::decorator_binding(ctx.workspace, scope, anno.name.name))
            .map_err(|message| CompilerError::new_with_label(anno.span, message))?
            .map(|resolved| resolved.key);
        if let Some(key) = resolved {
            let is_decorator = ctx
                .workspace
                .defs
                .get(&key)
                .is_some_and(|meta| meta.kind == bone_core::modules::DefKind::Decorator);
            if is_decorator {
                anno.resolved_name = Some(key);
            }
        }
        let is_wire_role = anno.name.name.as_str() == "wire"
            || anno.resolved_name.is_some_and(|key| {
                ctx.workspace
                    .defs
                    .get(&key)
                    .is_some_and(|meta| meta.name.as_str() == "wire")
            });
        if !is_wire_role {
            continue;
        }
        let Some(names) = anno.args.iter().find_map(|arg| {
            (arg.name
                .as_ref()
                .is_some_and(|name| name.name.as_str() == "onchange"))
            .then_some(match &arg.value {
                hir::AnnoValue::IdentList(names) => Some(names),
                _ => None,
            })
            .flatten()
        }) else {
            continue;
        };
        let mut resolved = Vec::with_capacity(names.len());
        for name in names {
            let identity = ctx.resolve_bare(name.name, name.span)?;
            resolved.push(identity.key);
        }
        anno.resolved_ident_list = Some(resolved);
    }
    Ok(lowered)
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

/// Capture effectful place components before duplicating the read and write.
fn compound_place_temps(a: &Ast, place: &Idx<ExprData>) -> Vec<Idx<ExprData>> {
    use skull_parser::ExprKind;

    fn walk(a: &Ast, e: &Idx<ExprData>, root: bool, out: &mut Vec<Idx<ExprData>>) {
        match &a.e(e).kind {
            ExprKind::Paren(o) => walk(a, o, root, out),
            ExprKind::FieldAccess(o, _) => walk(a, o, false, out),
            ExprKind::Index(o, i) => {
                walk(a, o, false, out);
                out.push(*i);
            }
            ExprKind::Ident(_) | ExprKind::Path(_) => {}
            // A non-place root is not assignable; leave it to the ordinary
            // diagnostics rather than assigning to a temporary.
            _ if root => {}
            _ => out.push(*e),
        }
    }

    let mut parts = Vec::new();
    walk(a, place, true, &mut parts);
    if parts.iter().all(|p| expr_is_pure(a, p)) {
        return Vec::new();
    }
    parts.retain(|p| !expr_is_literal(a, p));
    parts
}

fn lower_named_call(
    ctx: &mut LowerCtx,
    scope: Idx<Scope>,
    name: &skull_parser::Ident,
    args_range: IdxRange<Expr>,
    generic_args: IdxRange<TypeExpr>,
) -> Result<Expr, CompilerError> {
    let local = lookup_local(&ctx.body, scope, name.name);
    let (callee_key, family) = if local.is_none() && name.name.as_str() != "_" {
        // Keep an unbound call in HIR so inference can report the missing
        // function and editor actions can offer a stub.
        match ctx.resolve_optional_family(name.name, name.span) {
            Ok(resolved) => resolved,
            Err(e) => {
                ctx.recover_name(e)?;
                return Ok(Expr::Error);
            }
        }
    } else {
        (name.name, hir::CallFamily::Resolved(name.name))
    };
    let callee = match local {
        Some(binding) => hir::Callee::Local(binding),
        None if name.name.as_str() == "_" => match lookup_in_scope(ctx, scope, name)? {
            Expr::Var(binding) => hir::Callee::Local(binding),
            _ => hir::Callee::Unresolved {
                name: callee_key,
                name_span: ctx.body.name_spans.alloc(name.span),
            },
        },
        None => hir::Callee::Unresolved {
            name: callee_key,
            name_span: ctx.body.name_spans.alloc(name.span),
        },
    };

    Ok(Expr::Call {
        callee,
        args: args_range,
        generic_args,
        family,
    })
}

fn expr_is_literal(a: &Ast, e: &Idx<ExprData>) -> bool {
    use skull_parser::ExprKind;

    matches!(
        &a.e(e).kind,
        ExprKind::Int(..)
            | ExprKind::Float(_)
            | ExprKind::String(_)
            | ExprKind::Bool(_)
            | ExprKind::Null
            | ExprKind::Unit
    )
}

/// Whether evaluating `e` twice is indistinguishable from evaluating it once.
/// The AST has no operand types yet, so overloadable operators may call user code.
fn expr_is_pure(a: &Ast, e: &Idx<ExprData>) -> bool {
    use skull_parser::ExprKind;

    match &a.e(e).kind {
        ExprKind::Ident(_) | ExprKind::Path(_) => true,
        _ if expr_is_literal(a, e) => true,
        ExprKind::Paren(o) | ExprKind::FieldAccess(o, _) | ExprKind::BitNot(o) => {
            expr_is_pure(a, o)
        }
        ExprKind::Not(_) | ExprKind::UnaryMinus(_) => false,
        ExprKind::Index(o, i) => expr_is_pure(a, o) && expr_is_pure(a, i),
        ExprKind::BinOp {
            left, op, right, ..
        } => {
            !matches!(
                op,
                TokenKind::Assign
                    | TokenKind::AddAssign
                    | TokenKind::SubAssign
                    | TokenKind::MulAssign
                    | TokenKind::DivAssign
            ) && op.overloadable_binop_fn().is_none()
                && expr_is_pure(a, left)
                && expr_is_pure(a, right)
        }
        _ => false,
    }
}

/// `place op= value` whose place has effectful parts:
/// `{ let t1 = part1; ...; place' = place' op value }`, where `place'` reads
/// each part from its temporary.
#[allow(clippy::too_many_arguments)]
fn lower_compound_assign_with_temps(
    ctx: &mut LowerCtx,
    scope: Idx<Scope>,
    e: &ExprData,
    op_fn: &str,
    left: &Idx<ExprData>,
    right: &Idx<ExprData>,
    temps: &[Idx<ExprData>],
) -> Result<Expr, CompilerError> {
    let a = ctx.ast;
    let scope = ctx.body.alloc_child_scope(scope);
    let saved = ctx.place_temps.len();
    let result = (|| {
        let mut statements = Vec::with_capacity(temps.len() + 1);
        for part in temps {
            let part = a.e(part);
            let value = lower_expr(ctx, scope, part)?;
            let binding = ctx.body.binding_arena.alloc(BindingInfo {
                name: ustr("__place"),
                kind: BindingKind::Let,
                declared_ty: None,
                span: part.span,
            });
            ctx.body.binding_spans.insert(binding, part.span);
            let pattern = ctx.body.pattern_arena.alloc(Pattern::Bind(binding));
            statements.push((
                Statement::Let {
                    pattern,
                    value,
                    else_block: None,
                },
                part.span,
            ));
            ctx.place_temps
                .push((part as *const ExprData as usize, binding));
        }

        let target = lower_expr(ctx, scope, a.e(left))?;
        let (callee_key, family) = ctx.resolve_callee_family(ustr(op_fn), e.span)?;
        let args = lower_exprs(ctx, scope, [left, right].into_iter())?;
        let name_span = ctx.body.name_spans.alloc(e.span);
        let call = ctx.alloc_expr(
            Expr::Call {
                callee: Callee::Unresolved {
                    name: callee_key,
                    name_span,
                },
                args,
                generic_args: IdxRange::new_empty(),
                family,
            },
            e.span,
        );
        let assign = ctx.alloc_expr(
            Expr::Assign {
                target,
                value: call,
            },
            e.span,
        );
        statements.push((Statement::Expr(assign), e.span));

        let mut range: Option<(Idx<Statement>, Idx<Statement>)> = None;
        for (s, span) in statements {
            let id = ctx.body.alloc_statement(s, span);
            range = Some((range.map_or(id, |r| r.0), id));
        }
        let (first, last) = range.expect("at least the assignment statement");
        Ok(Expr::Block {
            statements: IdxRange::new_inclusive(first..=last),
            tail: None,
        })
    })();
    ctx.place_temps.truncate(saved);
    result
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
        | ExprKind::ParenAscription(i, _) => pl_chain(a, i),

        ExprKind::BinOp { left, right, .. } => pl_chain(a, left) || pl_chain(a, right),

        ExprKind::FunctionCall { func, .. } => pl_chain(a, func),

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

        ExprKind::FunctionCall { func, args, .. } => {
            pl_chain(a, func) || args.iter().any(|x| pl_is_us(a, &x.value))
        }

        ExprKind::Paren(i) => is_placeholder_lambda(a, a.e(i)),

        ExprKind::UnaryMinus(i)
        | ExprKind::Not(i)
        | ExprKind::BitNot(i)
        | ExprKind::QuestionMark(i)
        | ExprKind::As(i, _)
        | ExprKind::ParenAscription(i, _) => pl_chain(a, i),

        ExprKind::If { branches, .. } => branches.iter().any(|(c, _)| pl_is_us(a, c)),

        _ => false,
    }
}

fn lower_placeholder_lambda(
    ctx: &mut LowerCtx,
    scope: Idx<Scope>,
    e: &ExprData,
) -> Result<Expr, CompilerError> {
    let lambda_scope = ctx.body.alloc_child_scope(scope);
    let param = ctx.alloc_binding(
        BindingInfo {
            name: ustr("_"),
            kind: BindingKind::Param,
            declared_ty: None,
            span: e.span,
        },
        e.span,
    );

    let prev = ctx.placeholder_param.replace(param);

    let body_val = lower_expr_value(ctx, lambda_scope, e);
    ctx.placeholder_param = prev;

    let body_idx = ctx.alloc_expr(body_val?, e.span);

    Ok(Expr::Lambda {
        params: IdxRange::new_inclusive(param..=param),
        body: body_idx,
    })
}
