use skull_parser::{Ast, Ident, Item, ItemKind};
use bone_core::modules::{FragmentScope, NsNode, NsPackage, NsVisibility, UsePathRef};

use bone_core::annotations::{validate_annotations, AnnoPos};
use bone_lower::lower_annotations;

use crate::hir::{BuiltinTy, TypeCtor, TypeExpr, VariantPayload};
use crate::{hir::Module, prelude::*};

struct LowerItem<'a> {
    module: &'a mut Module,
    rock_module: &'a Ast,
    /// This file's modules-v1 scope, when it is a v1 fragment.
    v1_scope: Option<FragmentScope>,
}

/// Best-effort classification of a decl's trailing generic defaults for the
/// workspace map: walk from the last param backwards while each default
/// classifies as index-free (a builtin or a bare named type that is not a
/// sibling param), stop at the first param without one. Authoritative
/// validation with spans is bone-lower's `validate_generic_defaults` -- an
/// invalid decl stores a shorter (or empty) run here and rejects there.
fn trailing_generic_defaults(
    ast: &Ast,
    generics: &[skull_parser::Generic],
) -> Vec<bone_core::hir::GenericDefault> {
    use bone_core::hir::{BuiltinTy, GenericDefault};
    use skull_parser::TypeExprKind;

    let mut out = Vec::new();
    for g in generics.iter().rev() {
        let Some(d) = &g.default else { break };
        let te = ast.te(d);
        if te.is_nullable {
            break;
        }
        let classified = match &te.kind {
            TypeExprKind::Int => GenericDefault::Builtin(BuiltinTy::Int),
            TypeExprKind::Uint => GenericDefault::Builtin(BuiltinTy::Uint),
            TypeExprKind::Float => GenericDefault::Builtin(BuiltinTy::Float),
            TypeExprKind::Bool => GenericDefault::Builtin(BuiltinTy::Bool),
            TypeExprKind::String => GenericDefault::Builtin(BuiltinTy::String),
            TypeExprKind::Unit => GenericDefault::Builtin(BuiltinTy::Unit),
            TypeExprKind::Any => GenericDefault::Builtin(BuiltinTy::Any),
            TypeExprKind::Ident(n)
                if n.name.as_str() != "_"
                    && !generics.iter().any(|s| s.name.name == n.name) =>
            {
                GenericDefault::Named(n.name)
            }
            _ => break,
        };
        out.push(classified);
    }
    out.reverse();
    out
}

pub fn expand_and_desugar(ws: &mut Workspace, cooked: &mut [Ast]) -> Result<(), CompilerError> {
    let _span = tracing::info_span!("expand_and_desugar").entered();

    let timer = Instant::now();
    skull_parser::macro_expand::expand_macros(cooked);
    ws.log_time("macro_expand", timer);

    let timer = Instant::now();
    for ast in cooked.iter_mut() {
        skull_parser::desugar::desugar(ast);
    }
    ws.log_time("desugar", timer);

    Ok(())
}

pub fn lower_modules(
    ws: &mut Workspace,
    rock_modules: &[Ast],
) -> Result<Vec<ModuleId>, CompilerError> {
    let mut modules = rock_modules
        .iter()
        .map(|rock_mod| Module {
            file: rock_mod.file,
            ..Default::default()
        })
        .collect_vec();

    for m in modules.iter_mut() {
        m.id = ws.intern_module(m.file);
    }

    // Record nominal generic arity from the raw AST, workspace-wide, BEFORE
    // type lowering -- so the bare-generic check in `lower_type_expr_in` works
    // cross-module (other modules aren't in `ws.modules` yet during this
    // parallel lowering; this map is). Insert (not clear) so the incremental
    // single-module re-lower augments rather than wipes other modules' entries.
    for rock_mod in rock_modules.iter() {
        let scope = ws.fragment_scopes.get(&rock_mod.file).cloned();
        let mut walk = |item: &Item, path: &[ustr::Ustr]| -> Result<(), CompilerError> {
            // In v1 the workspace maps key by the canonical DefKey so two
            // same-named types in different logical modules stay distinct.
            let key = |name: &ustr::Ustr| -> ustr::Ustr {
                match &scope {
                    Some(sc) => bone_core::modules::def_key(
                        &sc.package_name.to_string(),
                        path,
                        name.as_str(),
                    ),
                    None => *name,
                }
            };
            match &item.kind {
                ItemKind::Struct(s) => {
                    ws.type_generic_arity.insert(key(&s.name.name), s.generics.len());
                    ws.type_generic_defaults
                        .insert(key(&s.name.name), trailing_generic_defaults(rock_mod, &s.generics));
                    ws.has_nobox_types |= s.is_nobox;
                }
                ItemKind::Enum(e) => {
                    ws.type_generic_arity.insert(key(&e.name.name), e.generics.len());
                    ws.type_generic_defaults
                        .insert(key(&e.name.name), trailing_generic_defaults(rock_mod, &e.generics));
                }
                ItemKind::Newtype(n) => {
                    ws.type_generic_arity.insert(key(&n.name.name), n.generics.len());
                    ws.type_generic_defaults
                        .insert(key(&n.name.name), trailing_generic_defaults(rock_mod, &n.generics));
                }
                ItemKind::Decorator(d) => {
                    ws.decorator_names.insert(key(&d.name.name));
                }
                _ => {}
            }
            Ok(())
        };
        let start = scope.as_ref().map(|s| s.module_path.clone()).unwrap_or_default();
        for_each_def_item(rock_mod.items.as_slice(), &start, &mut walk)?;
    }

    let mut items = modules
        .iter_mut()
        .zip(rock_modules.iter())
        .map(|(a, b)| LowerItem {
            module: a,
            rock_module: b,
            v1_scope: ws.fragment_scopes.get(&b.file).cloned(),
        })
        .collect_vec();

    // Pass 1: struct & fn name declarations
    {
        let _span = tracing::info_span!("forward_decls").entered();
        let timer = Instant::now();

        items
            .par_iter_mut()
            .map(|item| {
                pass_register_forward_decls(item.rock_module, item.module, item.v1_scope.as_ref())
            })
            .collect::<Result<Vec<()>, CompilerError>>()?;

        ws.log_time("register decls", timer);
    }

    // Annotation registry validation for NON-fn item positions
    // (docs/annotations-design.md). Fn items, params, and fields validate
    // during lowering (build_function + the field/variant loops), which also
    // covers nested/grafted fns. Runs AFTER forward decls so the more
    // specific rejections there (retired `@value`) fire first.
    for rock_mod in rock_modules.iter() {
        for item in rock_mod.items.iter() {
            if item.annotations.is_empty() {
                continue;
            }
            let pos = match &item.kind {
                ItemKind::Function(_) => continue,
                ItemKind::Struct(_) => AnnoPos::StructItem,
                ItemKind::Enum(_) => AnnoPos::EnumItem,
                ItemKind::Newtype(_) => AnnoPos::NewtypeItem,
                ItemKind::Global(_) => AnnoPos::GlobalItem,
                ItemKind::Decorator(_) => AnnoPos::DecoItem,
                ItemKind::CompilerDirective(_) | ItemKind::Comment(_) | ItemKind::MacroInvocation { .. } => continue,
                // Legacy use/mod are rejected in build_namespaces; v1 Module
                // blocks are walked via for_each_def_item, not as annotated items.
                ItemKind::Use(_) | ItemKind::Module(_) => continue,
            };
            let lowered = lower_annotations(rock_mod, &item.annotations);
            validate_annotations(pos, &lowered, &ws.decorator_names)?;
        }
    }

    {
        let _span = tracing::info_span!("name_reg").entered();
        let timer = Instant::now();

        for item in items.iter_mut() {
            let mid = item.module.module_id();

            // Cross-module duplicate names are a recoverable INTERFACE error, not a
            // fail-fast: record it in the offending module's `interface_errors` (the
            // same channel `lower_type_expr_in` uses for "Unknown type name") and
            // keep going, so the LSP/editor still gets partial results AND a
            // persistent squiggle. A `comp_error!` here aborted the whole pass, so
            // the error never reached `session.diagnostics()` -- it only surfaced
            // transiently as `update_file`'s `Err`, invisible to `diagnostics_for`.
            let mut dup_errs: Vec<CompilerError> = Vec::new();

            for (name, def) in item.module.struct_by_name.iter() {
                if ws
                    .struct_by_name
                    .insert(
                        *name,
                        StructId {
                            module: mid,
                            def: *def,
                        },
                    )
                    .is_some()
                {
                    let span = item.module.structs[*def].span;
                    dup_errs.push(CompilerError::new_with_label(
                        span,
                        format!("duplicate struct `{}` across modules", name),
                    ));
                }
            }

            for (name, def) in item.module.enums_by_name.iter() {
                if ws
                    .enums_by_name
                    .insert(
                        *name,
                        EnumId {
                            module: mid,
                            def: *def,
                        },
                    )
                    .is_some()
                {
                    let span = item.module.enums[*def].span;
                    dup_errs.push(CompilerError::new_with_label(
                        span,
                        format!("duplicate enum `{}` across modules", name),
                    ));
                }
            }

            for (name, def) in item.module.newtypes_by_name.iter() {
                if ws
                    .newtypes_by_name
                    .insert(
                        *name,
                        NewtypeId {
                            module: mid,
                            def: *def,
                        },
                    )
                    .is_some()
                {
                    let span = item.module.newtypes[*def].span;
                    dup_errs.push(CompilerError::new_with_label(
                        span,
                        format!("duplicate newtype `{}` across modules", name),
                    ));
                }
            }

            item.module.interface_errors.extend(dup_errs);
        }

        ws.log_time("register names", timer);
    }

    // Pass 2: field lowering
    let lower_field_timer = Instant::now();
    let ctxts = {
        let _span = tracing::info_span!("lower_fields").entered();

        items
            .par_iter_mut()
            .map(|item| {
                let mut ctx = LowerCtx {
                    workspace: ws,
                    module: item.module,
                    generics_in_scope: Default::default(),
                    _next_var_id: 0,
                    placeholder_param: None,
                    placeholder_forbidden: false,
                };

                pass_lower_fields(item.rock_module, &mut ctx)?;

                Ok::<_, CompilerError>((item.rock_module, ctx))
            })
            .collect::<Result<Vec<_>, _>>()?
    };

    let lower_field_time = lower_field_timer.elapsed();

    // Pass 3: function lowering
    {
        let _span = tracing::info_span!("lower_functions").entered();
        let timer = Instant::now();

        let modules = ctxts
            .into_par_iter()
            .map(|(rock_module, mut c)| {
                pass_lower_functions(rock_module, &mut c)?;

                c.module.lower_var_high_water = c._next_var_id;

                Ok::<_, CompilerError>(c.module)
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut all_fns_by_name: FxHashMap<Ustr, FnList> = FxHashMap::default();

        for module in modules.iter() {
            for (name, fns) in module.functions_by_name.clone().into_iter() {
                all_fns_by_name.entry(name).or_default().extend(fns);
            }
        }

        for (name, fns) in all_fns_by_name {
            ws.functions_by_name.entry(name).or_default().extend(fns);
        }

        ws.phase_timings.push(("lower fields", lower_field_time));
        ws.log_time("lower functions", timer);
    }

    // Register global names
    {
        let _span = tracing::info_span!("register_globals").entered();
        let timer = Instant::now();

        for module in modules.iter_mut() {
            let mut dup_errs: Vec<CompilerError> = Vec::new();
            for (_, g) in module.globals.iter() {
                if ws.globals_by_name.insert(g.name, g.ty).is_some() {
                    dup_errs.push(CompilerError::new_with_label(
                        g.span,
                        format!("duplicate global `{}` across modules", g.name),
                    ));
                }
                if g.is_const {
                    ws.const_globals.insert(g.name);
                }
            }
            module.interface_errors.extend(dup_errs);
        }

        ws.log_time("register global names", timer);
    }

    let ids: Vec<_> = modules.iter().map(|x| x.module_id()).collect_vec();

    // Globals are inferred per-body inside `check_module` (`check_global`),
    // which records type errors per `BodyOwner::Global` instead of failing the
    // whole pass -- so there is no standalone fail-fast global-inference block.
    for module in modules.into_iter() {
        ws.insert_module(module);
    }

    check_embed_site_legality(ws, &ids);
    check_frame_ref_storage(ws, &ids);

    Ok(ids)
}

fn static_type_contains_ref(ws: &Workspace, ty: TyId) -> bool {
    fn visit(ws: &Workspace, ty: TyId, seen: &mut FxHashSet<TyId>) -> bool {
        if !seen.insert(ty) {
            return false;
        }
        match ty {
            TyId::Local(_) => false,
            TyId::Static(mid, idx) => match ws.module(mid).ty_arena[idx] {
                TypeExpr::App {
                    ctor: TypeCtor::Ref,
                    ..
                } => true,
                TypeExpr::Nullable(inner) => visit(ws, ty.with_idx(inner), seen),
                TypeExpr::App { mut params, .. } => params.any(|p| visit(ws, ty.with_idx(p), seen)),
                TypeExpr::Function { mut params, ret } => {
                    params.any(|p| visit(ws, ty.with_idx(p), seen))
                        || visit(ws, ty.with_idx(ret), seen)
                }
                TypeExpr::Tuple { mut elements } => {
                    elements.any(|p| visit(ws, ty.with_idx(p), seen))
                }
                TypeExpr::Builtin(_) | TypeExpr::Generic(..) | TypeExpr::Infer(..) => false,
            },
        }
    }

    visit(ws, ty, &mut FxHashSet::default())
}

fn check_frame_ref_storage(ws: &mut Workspace, fresh: &[ModuleId]) {
    let mut errs = Vec::new();
    for module in ws.modules.iter() {
        if !fresh.contains(&module.id) {
            continue;
        }
        for (_, def) in module.structs.iter() {
            for field in &def.fields {
                if static_type_contains_ref(ws, field.ty) {
                    errs.push((
                        module.id,
                        CompilerError::new_with_label(
                            field.span,
                            format!(
                                "field `{}` cannot store a frame reference; store an `Entity<T>` or a value instead",
                                field.name
                            ),
                        ),
                    ));
                }
            }
        }
        for (_, def) in module.enums.iter() {
            for variant in &def.variants {
                match &variant.payload {
                    VariantPayload::Unit => {}
                    VariantPayload::Tuple(types) => {
                        if types.iter().any(|ty| static_type_contains_ref(ws, *ty)) {
                            errs.push((
                                module.id,
                                CompilerError::new_with_label(
                                    variant.name_span,
                                    format!(
                                        "enum variant `{}` cannot store a frame reference; store an `Entity<T>` or a value instead",
                                        variant.name
                                    ),
                                ),
                            ));
                        }
                    }
                    VariantPayload::Record(fields) => {
                        for field in fields {
                            if static_type_contains_ref(ws, field.ty) {
                                errs.push((
                                    module.id,
                                    CompilerError::new_with_label(
                                        field.span,
                                        format!(
                                            "enum field `{}` cannot store a frame reference; store an `Entity<T>` or a value instead",
                                            field.name
                                        ),
                                    ),
                                ));
                            }
                        }
                    }
                }
            }
        }
        for (_, def) in module.newtypes.iter() {
            if static_type_contains_ref(ws, def.wrapped) {
                errs.push((
                    module.id,
                    CompilerError::new_with_label(
                        def.span,
                        format!("newtype `{}` cannot store a frame reference", def.name),
                    ),
                ));
            }
        }
        for (_, global) in module.globals.iter() {
            if static_type_contains_ref(ws, global.ty) {
                errs.push((
                    module.id,
                    CompilerError::new_with_label(
                        global.span,
                        format!(
                            "global `{}` cannot store a frame reference; store an `Entity<T>` or a value instead",
                            global.name
                        ),
                    ),
                ));
            }
        }
        for (_, function) in module.functions.iter() {
            if !function.is_extern && function.inline_body.is_none() {
                continue;
            }
            let carries_ref = function
                .param_decls
                .iter()
                .any(|param| static_type_contains_ref(ws, param.ty))
                || function
                    .return_ty
                    .is_some_and(|ty| static_type_contains_ref(ws, ty));
            if carries_ref {
                errs.push((
                    module.id,
                    CompilerError::new_with_label(
                        function.span,
                        format!(
                            "foreign or inline function `{}` cannot carry a frame reference across an opaque boundary",
                            function.name
                        ),
                    ),
                ));
            }
        }
    }

    for (mid, err) in errs {
        ws.module_mut(mid).interface_errors.push(err);
    }
}

/// What an `embed f: T` field points at, once `T` is peeled of nullability.
enum EmbedTarget {
    Struct(StructId),
    /// Not embeddable: `embed` inlines a type's LEAVES into the parent, so the
    /// target must be a nominal struct whose fields are known at the
    /// declaration. The string names what was found, for the message.
    NotAStruct(&'static str),
}

fn embed_target(ws: &Workspace, ty: TyId) -> EmbedTarget {
    let TyId::Static(mid, mut idx) = ty else {
        return EmbedTarget::NotAStruct("a body-local type");
    };
    let arena = &ws.module(mid).ty_arena;
    // Peel `?`: `embed f: Foo?` is a presence slot over the same leaves.
    while let TypeExpr::Nullable(inner) = arena[idx] {
        idx = inner;
    }
    match arena[idx] {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(n),
            ..
        } => match ws.struct_by_name.get(&n) {
            Some(sid) => EmbedTarget::Struct(*sid),
            None => EmbedTarget::NotAStruct("a non-struct nominal type"),
        },
        TypeExpr::App {
            ctor: TypeCtor::Array,
            ..
        }
        | TypeExpr::App {
            ctor: TypeCtor::Map, ..
        } => EmbedTarget::NotAStruct("a collection"),
        TypeExpr::App {
            ctor: TypeCtor::Ref, ..
        } => EmbedTarget::NotAStruct("a frame reference"),
        TypeExpr::Builtin(BuiltinTy::Any) => EmbedTarget::NotAStruct("`any`"),
        TypeExpr::Builtin(_) => EmbedTarget::NotAStruct("a primitive"),
        TypeExpr::Generic(..) | TypeExpr::Infer(..) => {
            EmbedTarget::NotAStruct("a generic parameter")
        }
        TypeExpr::Function { .. } => EmbedTarget::NotAStruct("a function type"),
        TypeExpr::Tuple { .. } => EmbedTarget::NotAStruct("a tuple"),
        TypeExpr::Nullable(_) => EmbedTarget::NotAStruct("a non-struct type"),
    }
}

/// Declaration-level legality for `embed` fields, workspace-wide (a target can
/// live in another module, so this cannot run per-module during lowering).
///
/// Two rules, both from the storage model: the target must be a nominal struct
/// (embed inlines its LEAVES into the parent, so they must be statically
/// known), and the embed graph must be ACYCLIC -- a cycle has no finite layout,
/// and unlike a boxed self-reference there is no pointer to break it.
///
/// Recoverable `interface_errors`, not fail-fast: same channel as an unknown
/// type name in a signature, so the LSP still gets partial results. Errors are
/// recorded only for the modules lowered by THIS call (`fresh`) -- an unchanged
/// module keeps the entry it already carries, so an incremental edit cannot
/// accumulate duplicates of another module's error.
fn check_embed_site_legality(ws: &mut Workspace, fresh: &[ModuleId]) {
    let mut errs: Vec<(ModuleId, CompilerError)> = Vec::new();
    // struct -> the structs it embeds, for the acyclicity walk.
    let mut edges: FxHashMap<StructId, Vec<(StructId, Span, Ustr)>> = FxHashMap::default();

    for m in ws.modules.iter() {
        let mid = m.id;
        for (def, sdef) in m.structs.iter() {
            let sid = StructId { module: mid, def };
            for f in sdef.fields.iter() {
                if !f.is_embed {
                    continue;
                }
                ws.has_embed_fields = true;
                match embed_target(ws, f.ty) {
                    EmbedTarget::Struct(target) => {
                        edges.entry(sid).or_default().push((target, f.span, f.name));
                    }
                    EmbedTarget::NotAStruct(what) => errs.push((
                        mid,
                        CompilerError::new_with_label(
                            f.span,
                            format!(
                                "cannot `embed` field `{}`: {} has no statically known leaves to inline -- only a struct can be embedded",
                                f.name, what
                            ),
                        ),
                    )),
                }
            }
        }
    }

    // Any struct on a cycle reports at its own embed field, so a mutual cycle
    // names both sides rather than depending on visit order.
    for (&sid, fields) in edges.iter() {
        for &(target, span, fname) in fields.iter() {
            if reaches(&edges, target, sid) {
                errs.push((
                    sid.module,
                    CompilerError::new_with_label(
                        span,
                        format!(
                            "cannot `embed` field `{}`: the embed graph is cyclic, so the layout has no finite size -- box one link to break it",
                            fname
                        ),
                    ),
                ));
            }
        }
    }

    for (mid, e) in errs {
        if fresh.contains(&mid) {
            ws.module_mut(mid).interface_errors.push(e);
        }
    }
}

/// Is `goal` reachable from `from` through embed edges (inclusive of `from`)?
fn reaches(
    edges: &FxHashMap<StructId, Vec<(StructId, Span, Ustr)>>,
    from: StructId,
    goal: StructId,
) -> bool {
    let mut seen: FxHashSet<StructId> = FxHashSet::default();
    let mut stack = vec![from];
    while let Some(s) = stack.pop() {
        if s == goal {
            return true;
        }
        if !seen.insert(s) {
            continue;
        }
        if let Some(next) = edges.get(&s) {
            stack.extend(next.iter().map(|(t, _, _)| *t));
        }
    }
    false
}

pub fn infer_modules(ws: &mut Workspace, ids: &[ModuleId]) -> Result<(), CompilerError> {
    // Pass 4: inference
    {
        let _span = tracing::info_span!("infer_functions").entered();
        let timer = Instant::now();

        let inference_results = ids
            .par_iter()
            .map(|id| {
                let _m = tracing::info_span!("check_module", module = %id.0).entered();
                Ok((*id, check_module(ws, *id, ws.module(*id))?))
            })
            .collect::<Result<Vec<_>, CompilerError>>()?;

        for (key, inf) in inference_results.into_iter() {
            ws.set_inference(key, inf);
        }

        ws.log_time("infer functions", timer);
    }

    Ok(())
}

pub fn lower_and_infer(
    ws: &mut Workspace,
    rock_modules: &[Ast],
) -> Result<Vec<ModuleId>, CompilerError> {
    let ids = lower_modules(ws, rock_modules)?;

    infer_modules(ws, &ids)?;

    Ok(ids)
}

pub fn semantic_pass(rock_modules: &mut [Ast]) -> Result<Workspace, CompilerError> {
    semantic_pass_with_aliases(rock_modules, &bone_core::modules::NsAliases::default())
}

/// [`semantic_pass`] with the host graph's dependency-alias tables, so a v1
/// package resolves its dependencies under the names its manifest declared.
/// Legacy-only workspaces pass the default (empty) input.
pub fn semantic_pass_with_aliases(
    rock_modules: &mut [Ast],
    aliases: &bone_core::modules::NsAliases,
) -> Result<Workspace, CompilerError> {
    let _span = tracing::info_span!("bone_semantic_pass").entered();
    let mut ws = Workspace::default();

    build_namespaces(&mut ws, rock_modules, aliases)?;
    expand_and_desugar(&mut ws, rock_modules)?;
    lower_and_infer(&mut ws, rock_modules)?;

    Ok(ws)
}

/// Legacy compilation units parse `use`/`mod` (contextual, additive) but must
/// still reject them with a named diagnostic: v1 resolution does not apply,
/// and silently accepting the item would drop the declaration with no effect.
fn reject_legacy_unscoped_module_items(rock_mod: &Ast) -> Result<(), CompilerError> {
    if let Some(h) = &rock_mod.module_header {
        return Err(CompilerError::new_with_label(
            h.span,
            "module file headers require module resolution (not yet implemented)".to_string(),
        ));
    }
    for item in rock_mod.items.iter() {
        match &item.kind {
            ItemKind::Use(u) => {
                return Err(CompilerError::new_with_label(
                    u.span,
                    "`use` declarations require module resolution (not yet implemented)".to_string(),
                ));
            }
            ItemKind::Module(m) => {
                return Err(CompilerError::new_with_label(
                    m.span,
                    "inline `mod` blocks require module resolution (not yet implemented)".to_string(),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Walk every definition-bearing item, descending into inline mod blocks.
/// `path` is the logical module path within the file's package; `in_v1`
/// reports whether the containing file is a modules-v1 fragment.
fn for_each_def_item<'a>(
    items: &'a [Item],
    path: &[ustr::Ustr],
    f: &mut impl FnMut(&'a Item, &[ustr::Ustr]) -> Result<(), CompilerError>,
) -> Result<(), CompilerError> {
    for item in items {
        match &item.kind {
            ItemKind::Module(m) => {
                let mut child = path.to_vec();
                child.push(m.name.name);
                for_each_def_item(&m.items, &child, f)?;
            }
            _ => f(item, path)?,
        }
    }
    Ok(())
}

/// The modules-v1 namespace builder (plan M2). Runs before any lowering:
/// it turns the v1 fragments' headers, inline mod blocks, and use
/// declarations into the workspace's namespace tree, registers every v1
/// definition under its canonical DefKey, and records per-fragment import
/// scopes for the resolver. Legacy files contribute nothing here and keep
/// today's bare-name keys and diagnostics.
fn build_namespaces(
    ws: &mut Workspace,
    rock_modules: &[Ast],
    aliases: &bone_core::modules::NsAliases,
) -> Result<(), CompilerError> {
    // The host graph's alias tables land on the tree first, so a package
    // resolves its dependencies under its manifest's names even when no v1
    // file of the provider is being lowered yet.
    for input in &aliases.packages {
        let pkg = ws.ns.packages.entry(input.id).or_insert_with(|| NsPackage {
            id: input.id,
            self_name: input.self_name,
            ..Default::default()
        });
        for (alias, target) in &input.aliases {
            pkg.aliases.insert(*alias, *target);
        }
    }
    for rock_mod in rock_modules.iter() {
        let is_v1 = rock_mod.dialect == skull_parser::Dialect::V1;
        if !is_v1 {
            reject_legacy_unscoped_module_items(rock_mod)?;
            continue;
        }
        let Some(tag) = rock_mod.package.clone() else {
            // A v1 file without a package tag: standalone sources must
            // not silently become legacy — that would reinterpret their
            // `::` ascriptions. Reject with the reason.
            return Err(CompilerError::new_with_label(
                rock_mod
                    .module_header
                    .as_ref()
                    .map(|h| h.span)
                    .unwrap_or_else(crate::prelude::Span::unknown),
                "modules-v1 source has no owning package (standalone v1 sources need a package or script context)".to_string(),
            ));
        };
        let pkg_id = tag.id;
        // Ensure the package exists in the tree.
        ws.ns
            .packages
            .entry(pkg_id)
            .or_insert_with(|| NsPackage {
                id: pkg_id,
                self_name: ustr::Ustr::from(&tag.self_name),
                ..Default::default()
            });

        let module_path: Vec<ustr::Ustr> = rock_mod
            .module_header
            .as_ref()
            .map(|h| h.path.iter().map(|i| i.name).collect())
            .unwrap_or_default();

        let mut scope = FragmentScope {
            package: pkg_id,
            package_name: ustr::Ustr::from(&tag.self_name),
            module_path: module_path.clone(),
            ..Default::default()
        };

        // Walk the file's items (recursing into inline mod blocks),
        // registering owned definitions and collecting use declarations.
        let file = rock_mod.file;
        let mut errs: Vec<CompilerError> = Vec::new();
        if let Some(h) = &rock_mod.module_header {
            if let Some(vis) = &h.vis {
                let ns_vis = to_ns_vis(vis);
                let pkg = ws.ns.packages.get_mut(&pkg_id).expect("package ensured");
                let key = NsPackage::path_key(&module_path);
                let node = pkg.nodes.entry(key).or_insert_with(|| NsNode {
                    path_key: key,
                    segments: module_path.clone(),
                    ..Default::default()
                });
                if let Some(existing) = &node.vis {
                    if *existing != ns_vis {
                        return Err(CompilerError::new_with_label(
                            h.span,
                            format!(
                                "conflicting visibility for module `{}`",
                                module_path
                                    .iter()
                                    .map(|s| s.as_str())
                                    .collect::<Vec<_>>()
                                    .join("::")
                            ),
                        ));
                    }
                }
                node.vis = Some(ns_vis);
            }
        }

        register_items(ws, pkg_id, &module_path, &rock_mod.items, &mut scope, &mut errs);
        if !errs.is_empty() {
            errs.sort_by_key(|e| e.location.offset);
            return Err(errs.remove(0));
        }

        ws.fragment_scopes.insert(file, scope);
    }
    Ok(())
}

/// Register one file's items into the namespace tree under `path`,
/// recursing into inline mod blocks. `scope` collects the fragment's own
/// imports; shared (`pub`/`private`-prefixed) use declarations become node
/// exports (spec 8.2).
fn register_items(
    ws: &mut Workspace,
    pkg_id: u32,
    path: &[ustr::Ustr],
    items: &[Item],
    scope: &mut FragmentScope,
    errs: &mut Vec<CompilerError>,
) {
    let ensure_node = |ws: &mut Workspace, path: &[ustr::Ustr]| -> ustr::Ustr {
        let pkg = ws.ns.packages.get_mut(&pkg_id).expect("package ensured");
        let key = NsPackage::path_key(path);
        pkg.nodes.entry(key).or_insert_with(|| NsNode {
            path_key: key,
            segments: path.to_vec(),
            ..Default::default()
        });
        key
    };

    // Ensure ancestors exist so mid-path traversal always lands, and
    // record each segment as a child of its parent (file headers create
    // nodes without an inline `mod` item).
    for i in 1..=path.len() {
        let child_key = ensure_node(ws, &path[..i]);
        let parent_key = ensure_node(ws, &path[..i - 1]);
        let seg = path[i - 1];
        let pkg = ws.ns.packages.get_mut(&pkg_id).expect("package ensured");
        if let Some(parent) = pkg.nodes.get_mut(&parent_key) {
            parent.children.entry(seg).or_insert(child_key);
        }
    }

    for item in items {
        match &item.kind {
            ItemKind::Module(m) => {
                let mut child = path.to_vec();
                child.push(m.name.name);
                // Record the child on the parent node.
                {
                    let parent_key = NsPackage::path_key(path);
                    let pkg = ws.ns.packages.get_mut(&pkg_id).expect("package");
                    let parent = pkg.nodes.entry(parent_key).or_insert_with(|| NsNode {
                        path_key: parent_key,
                        segments: path.to_vec(),
                        ..Default::default()
                    });
                    if parent.owned_types.contains_key(&m.name.name)
                        || parent.owned_globals.contains_key(&m.name.name)
                        || parent.owned_callables.contains_key(&m.name.name)
                        || parent.owned_decorators.contains_key(&m.name.name)
                    {
                        errs.push(CompilerError::new_with_label(
                            item.span,
                            format!(
                                "duplicate definition `{}` in module `{}`",
                                m.name.name,
                                if path.is_empty() {
                                    scope.package_name.to_string()
                                } else {
                                    path.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("::")
                                }
                            ),
                        ));
                    } else {
                        parent.children.insert(m.name.name, NsPackage::path_key(&child));
                    }
                }
                // Child visibility from the block header (`pub mod x { .. }`).
                if let Some(vis) = item.vis.clone() {
                    let child_key = NsPackage::path_key(&child);
                    let pkg = ws.ns.packages.get_mut(&pkg_id).expect("package");
                    let node = pkg.nodes.entry(child_key).or_insert_with(|| NsNode {
                        path_key: child_key,
                        segments: child.clone(),
                        ..Default::default()
                    });
                    let ns_vis = to_ns_vis(&vis);
                    if let Some(existing) = &node.vis {
                        if *existing != ns_vis {
                            errs.push(CompilerError::new_with_label(
                                item.span,
                                format!(
                                    "conflicting visibility for module `{}`",
                                    m.name.name
                                ),
                            ));
                            continue;
                        }
                    }
                    node.vis = Some(ns_vis);
                }
                register_items(ws, pkg_id, &child, &m.items, scope, errs);
            }
            ItemKind::Use(u) => {
                let to_ref = |from_root: bool, segments: &[Ident]| UsePathRef {
                    from_root,
                    segments: segments.iter().map(|i| i.name).collect(),
                };
                match &u.tree {
                    skull_parser::UseTree::Path { from_root, segments, kind } => {
                        match kind {
                            skull_parser::UseSuffix::Bind => {
                                // `use a::b;` binds the tail segment name.
                                let local = segments
                                    .last()
                                    .map(|i| i.name)
                                    .unwrap_or(ustr::Ustr::from(""));
                                if u.vis.is_some() {
                                    share_export(ws, pkg_id, path, local, to_ref(*from_root, segments), false, u, errs);
                                } else {
                                    scope.named.insert(local, to_ref(*from_root, segments));
                                }
                            }
                            skull_parser::UseSuffix::BindAs(alias) => {
                                if u.vis.is_some() {
                                    share_export(ws, pkg_id, path, alias.name, to_ref(*from_root, segments), false, u, errs);
                                } else {
                                    scope.named.insert(alias.name, to_ref(*from_root, segments));
                                }
                            }
                            skull_parser::UseSuffix::Glob => {
                                let _ = to_ref;
                                if u.vis.is_some() {
                                    share_export(
                                        ws,
                                        pkg_id,
                                        path,
                                        ustr::Ustr::from("*"),
                                        UsePathRef {
                                            from_root: *from_root,
                                            segments: segments.iter().map(|i| i.name).collect(),
                                        },
                                        true,
                                        u,
                                        errs,
                                    );
                                } else {
                                    scope.globs.push(UsePathRef {
                                        from_root: *from_root,
                                        segments: segments.iter().map(|i| i.name).collect(),
                                    });
                                }
                            }
                            skull_parser::UseSuffix::Operator(op) => {
                                let local = op.name;
                                let mut segs: Vec<Ident> = segments.clone();
                                segs.push(op.clone());
                                if u.vis.is_some() {
                                    share_export(ws, pkg_id, path, local, to_ref(*from_root, &segs), false, u, errs);
                                } else {
                                    scope.named.insert(local, to_ref(*from_root, &segs));
                                }
                            }
                            skull_parser::UseSuffix::Nested(trees) => {
                                // Flatten `use a::b::{ .. }` into per-subtree
                                // fragment bindings/exports.
                                for tree in trees {
                                    flatten_use_tree(ws, pkg_id, path, *from_root, segments, tree, scope, u, errs);
                                }
                            }
                        }
                    }
                }
            }
            other => {
                // Ordinary definitions: register under the DefKey on the node.
                let Some((bare, kind)) = owned_name_and_kind(other) else { continue };
                if path.is_empty() {
                    let pkg = ws.ns.packages.get(&pkg_id).expect("package");
                    if bare == pkg.self_name || pkg.aliases.contains_key(&bare) {
                        errs.push(CompilerError::new_with_label(
                            item.span,
                            format!(
                                "root declaration `{bare}` collides with a package or dependency alias"
                            ),
                        ));
                        continue;
                    }
                }
                let pkg = ws.ns.packages.get_mut(&pkg_id).expect("package");
                let key = NsPackage::path_key(path);
                let node = pkg.nodes.entry(key).or_insert_with(|| NsNode {
                    path_key: key,
                    segments: path.to_vec(),
                    ..Default::default()
                });
                let def_key = bone_core::modules::def_key(
                    &scope.package_name.to_string(),
                    path,
                    bare.as_str(),
                );
                let taken = node.owned_types.contains_key(&bare)
                    || node.owned_globals.contains_key(&bare)
                    || node.owned_callables.contains_key(&bare)
                    || node.owned_decorators.contains_key(&bare)
                    || node.children.contains_key(&bare);
                if taken {
                    errs.push(CompilerError::new_with_label(
                        item.span,
                        format!(
                            "duplicate definition `{bare}` in module `{}`",
                            if path.is_empty() {
                                scope.package_name.to_string()
                            } else {
                                path.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("::")
                            }
                        ),
                    ));
                    continue;
                }
                let table = match kind {
                    "type" => &mut node.owned_types,
                    "global" => &mut node.owned_globals,
                    "callable" => &mut node.owned_callables,
                    _ => &mut node.owned_decorators,
                };
                table.insert(bare, def_key);
                let def_kind = match kind {
                    "type" => bone_core::modules::DefKind::Type,
                    "global" => bone_core::modules::DefKind::Global,
                    "callable" => bone_core::modules::DefKind::Callable,
                    _ => bone_core::modules::DefKind::Decorator,
                };
                ws.defs.insert(
                    def_key,
                    bone_core::modules::DefMeta {
                        package: pkg_id,
                        module: path.to_vec(),
                        kind: def_kind,
                        name: bare,
                        vis: to_ns_vis(&item.vis.clone().unwrap_or(skull_parser::Visibility::Private)),
                        span: item.span,
                        dialect: skull_parser::Dialect::V1,
                    },
                );
            }
        }
    }
}

/// Flatten one subtree of `use a::b::{ .. }` into fragment bindings/exports.
fn flatten_use_tree(
    ws: &mut Workspace,
    pkg_id: u32,
    path: &[ustr::Ustr],
    from_root: bool,
    prefix: &[Ident],
    tree: &skull_parser::UseTree,
    scope: &mut FragmentScope,
    decl: &skull_parser::UseDecl,
    errs: &mut Vec<CompilerError>,
) {
    // Subtrees parse with `allow_root = false` (`parse_use_tree`), so a nested
    // tree never carries its own root anchor -- the declaration's anchor rides
    // the `from_root` parameter.
    let skull_parser::UseTree::Path { segments, kind, .. } = tree;
    let mut full: Vec<Ident> = prefix.to_vec();
    full.extend(segments.iter().cloned());
    match kind {
        skull_parser::UseSuffix::Bind => {
            let local = full.last().map(|i| i.name).unwrap_or(ustr::Ustr::from(""));
            scope.named.insert(
                local,
                UsePathRef { from_root, segments: full.iter().map(|i| i.name).collect() },
            );
        }
        skull_parser::UseSuffix::BindAs(alias) => {
            scope.named.insert(
                alias.name,
                UsePathRef { from_root, segments: full.iter().map(|i| i.name).collect() },
            );
        }
        skull_parser::UseSuffix::Operator(op) => {
            let local = op.name;
            let mut segs: Vec<Ustr> = full.iter().map(|i| i.name).collect();
            segs.push(local);
            scope.named.insert(
                local,
                UsePathRef { from_root, segments: segs },
            );
        }
        skull_parser::UseSuffix::Glob => {
            scope.globs.push(UsePathRef {
                from_root,
                segments: full.iter().map(|i| i.name).collect(),
            });
        }
        skull_parser::UseSuffix::Nested(trees) => {
            for tree in trees {
                flatten_use_tree(ws, pkg_id, path, from_root, &full, tree, scope, decl, errs);
            }
        }
    }
}

/// Record a shared export binding on the fragment's module node.
fn share_export(
    ws: &mut Workspace,
    pkg_id: u32,
    path: &[ustr::Ustr],
    local: ustr::Ustr,
    target: UsePathRef,
    is_glob: bool,
    decl: &skull_parser::UseDecl,
    errs: &mut Vec<CompilerError>,
) {
    let vis = decl.vis.clone();
    let binding = bone_core::modules::ExportBinding {
        target,
        is_glob,
        vis: to_ns_vis(&vis.unwrap_or(skull_parser::Visibility::Private)),
    };
    let pkg = ws.ns.packages.get_mut(&pkg_id).expect("package");
    let key = NsPackage::path_key(path);
    let node = pkg.nodes.entry(key).or_insert_with(|| NsNode {
        path_key: key,
        segments: path.to_vec(),
        ..Default::default()
    });
    if node.exports.insert(local, binding).is_some() {
        errs.push(CompilerError::new_with_label(
            decl.span,
            format!("duplicate export binding `{}`", local),
        ));
    }
}

fn owned_name_and_kind(item: &ItemKind) -> Option<(ustr::Ustr, &'static str)> {
    match item {
        ItemKind::Struct(s) => Some((s.name.name, "type")),
        ItemKind::Enum(e) => Some((e.name.name, "type")),
        ItemKind::Newtype(n) => Some((n.name.name, "type")),
        ItemKind::Global(g) => Some((g.name.name, "global")),
        ItemKind::Function(f) => Some((f.name.name, "callable")),
        ItemKind::Decorator(d) => Some((d.name.name, "decorator")),
        _ => None,
    }
}

fn to_ns_vis(vis: &skull_parser::Visibility) -> NsVisibility {
    NsVisibility::from_parser(vis)
}

pub fn check_module_incremental(
    ws: &Workspace,
    mid: ModuleId,
    module: &Module,
    mut prev: InferenceResult,
) -> Result<InferenceResult, CompilerError> {
    let mut result = InferenceResult::default();

    for (f_id, f) in module.functions.iter() {
        let owner = BodyOwner::Function(f_id);

        // Didn't lower
        let Some(body) = module.bodies.get(&owner) else {
            continue;
        };

        if reuse_body(&mut prev, &mut result, owner, body) {
            continue;
        }

        // A real re-inference (not a reuse): bump the workspace counter so the
        // incremental Session can report exactly how many bodies re-inferred.
        ws.reinferred_body_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        fold_inference(&mut result, owner, check_function(ws, module, mid, f_id, f));
    }

    for (d_id, d) in module.decorators.iter() {
        fold_inference(
            &mut result,
            BodyOwner::Decorator(d_id),
            check_decorator(ws, module, mid, d_id, d),
        );
    }

    for (g_id, g) in module.globals.iter() {
        fold_inference(
            &mut result,
            BodyOwner::Global(g_id),
            check_global(ws, module, mid, g_id, g),
        );
    }

    for (s_id, _) in module.structs.iter() {
        let key = BodyOwner::StructDefaults(s_id);
        if !module.bodies.contains_key(&key) {
            continue;
        }
        fold_inference(
            &mut result,
            key,
            check_struct_defaults(ws, module, mid, s_id),
        );
    }

    Ok(result)
}

pub fn check_module(
    ws: &Workspace,
    mid: ModuleId,
    module: &Module,
) -> Result<InferenceResult, CompilerError> {
    check_module_incremental(ws, mid, module, InferenceResult::default())
}

fn fold_inference(
    result: &mut InferenceResult,
    owner: BodyOwner,
    checked: Result<Option<(BodyOwner, BodyInference, Vec<CompilerError>)>, CompilerError>,
) {
    match checked {
        Ok(Some((o, inf, errs))) => {
            result.per_fn.insert(o, inf);

            if !errs.is_empty() {
                result.errors.insert(o, errs);
            }
        }

        Ok(None) => {}

        Err(err) => {
            result.errors.insert(owner, vec![err]);
        }
    }
}

fn reuse_body(
    prev: &mut InferenceResult,
    result: &mut InferenceResult,
    owner: BodyOwner,
    body: &Body,
) -> bool {
    if prev.errors.contains_key(&owner) {
        return false;
    }

    if prev
        .per_fn
        .get(&owner)
        .is_some_and(|bi| bi.body_generation == body.generation)
    {
        result
            .per_fn
            .insert(owner, prev.per_fn.remove(&owner).unwrap());

        if let Some(e) = prev.errors.remove(&owner) {
            result.errors.insert(owner, e);
        }

        true
    } else {
        false
    }
}

pub fn pass_register_forward_decls(
    rock_module: &Ast,
    module: &mut Module,
    v1_scope: Option<&FragmentScope>,
) -> Result<(), CompilerError> {
    // pass 1: register structs for forward decls. In a v1 fragment the
    // registration key (and the HIR definition name) is the canonical
    // DefKey, so per-logical-module duplicate checks fall out of the
    // workspace merge while same-named types in different modules stay
    // distinct.
    let start = v1_scope.map(|s| s.module_path.clone()).unwrap_or_default();
    for_each_def_item(rock_module.items.as_slice(), &start, &mut |item, path| {
        let def_key = |name: &ustr::Ustr| -> ustr::Ustr {
            match v1_scope {
                Some(sc) => bone_core::modules::def_key(
                    &sc.package_name.to_string(),
                    path,
                    name.as_str(),
                ),
                None => *name,
            }
        };
        if let ItemKind::Struct(s) = &item.kind {
            // `@value struct` is a retired form: the annotation set `is_value`
            // without `is_c_repr` (a codegen drift). Only the `value Foo { ... }`
            // keyword is canonical.
            if let Some(anno) = item.annotations.iter().find(|a| a.name.name == "value") {
                comp_error!(
                    anno.span,
                    "`@value` annotation is retired -- declare a value struct with the `value` keyword instead"
                );
            }
            let reg_name = def_key(&s.name.name);
            let idx = module.structs.alloc(hir::StructDef {
                name: reg_name,
                name_span: s.name.span,
                is_value: s.is_value,
                is_nobox: s.is_nobox,
                span: item.span,
                generics: Vec::new(),
                fields: Vec::new(),
            });

            if module
                .struct_by_name
                .insert(reg_name, idx)
                .is_some()
            {
                comp_error!(item.span, "duplicate struct definition: `{}`", s.name.name);
            }
        } else if let ItemKind::Enum(e) = &item.kind {
            let reg_name = def_key(&e.name.name);
            let idx = module.enums.alloc(hir::EnumDef {
                name: reg_name,
                name_span: e.name.span,
                span: item.span,
                generics: Vec::new(),
                variants: Vec::new(),
            });

            if module.enums_by_name.insert(reg_name, idx).is_some() {
                comp_error!(item.span, "duplicate enum definition: `{}`", e.name.name);
            }
        } else if let ItemKind::Newtype(n) = &item.kind {
            let placeholder = module
                .ty_arena
                .alloc(hir::TypeExpr::Builtin(hir::BuiltinTy::Unit));
            let wrapped = TyId::Static(module.module_id(), placeholder);

            let reg_name = def_key(&n.name.name);
            let idx = module.newtypes.alloc(NewtypeDef {
                name: reg_name,
                name_span: n.name.span,
                span: item.span,
                generics: Vec::new(),
                wrapped,
            });

            if module.newtypes_by_name.insert(reg_name, idx).is_some() {
                comp_error!(item.span, "duplicate newtype definition: `{}`", n.name.name);
            }
        }
        Ok(())
        })?;
    Ok(())
}

pub fn pass_lower_fields(a: &Ast, ctx: &mut LowerCtx) -> Result<(), CompilerError> {
    // pass 2: lower fields. v1 fragments register under their DefKeys, so
    // the walk carries each item's logical module path.
    let scope = ctx.v1_scope().cloned();
    let reg_key = |name: &ustr::Ustr, path: &[ustr::Ustr]| -> ustr::Ustr {
        match &scope {
            Some(sc) => {
                bone_core::modules::def_key(&sc.package_name.to_string(), path, name.as_str())
            }
            None => *name,
        }
    };
    let start = scope.as_ref().map(|s| s.module_path.clone()).unwrap_or_default();
    for_each_def_item(a.items.as_slice(), &start, &mut |item, path| {
        match &item.kind {
            ItemKind::Struct(s) => lower_struct_def(ctx, a, s, reg_key(&s.name.name, path))?,
            ItemKind::Enum(e) => lower_enum_def(ctx, a, e, reg_key(&e.name.name, path))?,
            ItemKind::Newtype(n) => lower_newtype_def(ctx, a, n, reg_key(&n.name.name, path))?,
            _ => {}
        }
        Ok(())
    })?;

    Ok(())
}

pub fn pass_lower_functions(a: &Ast, ctx: &mut LowerCtx) -> Result<(), CompilerError> {
    // pass 3: lower function defs
    let scope = ctx.v1_scope().cloned();
    let reg_key = |name: &ustr::Ustr, path: &[ustr::Ustr]| -> ustr::Ustr {
        match &scope {
            Some(sc) => {
                bone_core::modules::def_key(&sc.package_name.to_string(), path, name.as_str())
            }
            None => *name,
        }
    };
    let start = scope.as_ref().map(|s| s.module_path.clone()).unwrap_or_default();
    for_each_def_item(a.items.as_slice(), &start, &mut |item, path| {
        match &item.kind {
            ItemKind::Function(f) => {
                if f.is_macro {
                    comp_error!(item.span, "macro `{}` survived expansion", f.name.name);
                }

                // A definition for an operator nothing dispatches by name can
                // never be called, so accepting it is a swallow: `operator ==`
                // used to compile clean and leave structural equality in place
                // with no diagnostic anywhere, and the ordering ops failed at
                // the USE site blaming the operand type rather than naming the
                // ignored overload.
                //
                // The exemption is a declaration over PRIMITIVES, which is what
                // the hardcoded implementations these names refer to actually
                // are: the stdlib's `operator(lua) ==`/`<`/`&&`/`~` over int,
                // float, string and bool, and `fn __op_eq(a: any, b: any)`, the
                // structural equality codegen emits. Overloading for your OWN
                // type is the thing that can never be dispatched.
                //
                // Three narrower guards were tried and measured wrong. Gating on
                // a Rock body, or on the `operator` spelling, each let a user's
                // `inline(lua) operator ==` or plain `fn __op_eq` through to be
                // silently ignored -- the exact bug being fixed. Gating on the
                // module's file id looked right and is a bad proxy: `bone-query`
                // warms the stdlib from its real filesystem path, so the check
                // called the stdlib a user module and rejected its own
                // operators, taking 6 LSP completion suites with it.
                //
                // `is_synthetic` is load-bearing under this rule: `desugar`
                // mints `__op_lt`/`__op_le`/`__op_gt`/`__op_ge` over the NEWTYPE
                // (not a primitive), so every newtype over a primitive would
                // reject without it.
                let fn_name = f.name.name.as_str();
                let over_primitives = f.params.iter().all(|p| {
                    matches!(
                        a.types[p.ty].kind,
                        skull_parser::TypeExprKind::Int
                            | skull_parser::TypeExprKind::Uint
                            | skull_parser::TypeExprKind::Float
                            | skull_parser::TypeExprKind::String
                            | skull_parser::TypeExprKind::Bool
                            | skull_parser::TypeExprKind::Any
                    )
                });
                // Only names the table CLASSIFIES are judged. The `__op_`
                // namespace is wider than the parser's operator symbols --
                // `__op_null_coalesce` (`??`) is a plain generic stdlib fn with
                // no `operator` spelling -- and rejecting an unclassified name
                // here would fail the whole stdlib. Keeping the table total over
                // the parser is `operator_table_matches_the_parser`'s job, not a
                // runtime arm's.
                if !f.is_synthetic && !over_primitives {
                    if let Some((symbol, false)) = bone_core::hir::operator_entry(fn_name) {
                        comp_error!(
                            item.span,
                            "operator `{}` is not overloadable; only `{}` may be overloaded",
                            symbol,
                            bone_core::hir::overloadable_operator_symbols()
                        );
                    }
                }

                // Compiler/stdlib operator families (synthetic desugar and
                // primitive overloads) keep a bare `__op_*` key so the existing
                // intrinsic pipeline still finds them. User-declared operators
                // in v1 register under their DefKey and are selected through
                // imports / `operator(+)` (spec 8.5) -- not global discovery.
                let key = if fn_name.starts_with("__op_") && (f.is_synthetic || over_primitives) {
                    f.name.name
                } else {
                    reg_key(&f.name.name, path)
                };
                let fn_id = lower_function_def(
                    ctx,
                    a,
                    f,
                    &item.annotations,
                    item.span.offset_range.0,
                    key,
                )?;
                let mid = ctx.module.module_id();

                ctx.module
                    .functions_by_name
                    .entry(key)
                    .or_default()
                    .push((mid, fn_id));
            }

            ItemKind::Decorator(d) => {
                lower_decorator_def(ctx, a, d, &item.annotations)?;
            }

            ItemKind::Comment(_)
            | ItemKind::MacroInvocation { .. }
            | ItemKind::Struct(_)
            | ItemKind::Enum(_)
            | ItemKind::Newtype(_) => {}

            // v1: registered in build_namespaces. Legacy: rejected there.
            // Module items are walked by for_each_def_item, not passed here.
            ItemKind::Use(_) | ItemKind::Module(_) => {}

            ItemKind::Global(g) => {
                lower_global(ctx, a, g, &item.annotations, reg_key(&g.name.name, path))?;
            }

            ItemKind::CompilerDirective(_) => {
                comp_error!(item.span, "Unsupported top level item {:?}", item);
            }
        }
        Ok(())
    })?;
    Ok(())
}
