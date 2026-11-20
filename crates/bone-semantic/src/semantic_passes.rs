use bone_core::modules::{
    DefKind, FragmentScope, NsNode, NsPackage, NsVisibility, STD_PACKAGE_ID, STD_PACKAGE_NAME,
    UsePathRef, is_stdlib_module_file,
};
use skull_parser::stdlib::ModuleSet;
use skull_parser::{Ast, Item, ItemKind};

use bone_core::annotations::{AnnoPos, validate_annotations};
use bone_lower::{LowerCtx, lower_annotations_in_scope};

use crate::hir::{BuiltinTy, TypeCtor, TypeExpr, VariantPayload};
use crate::{hir::Module, prelude::*};

/// Record index-free trailing defaults. Lowering validates them and reports spans.
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
            TypeExprKind::Float | TypeExprKind::F64 => GenericDefault::Builtin(BuiltinTy::Float),
            TypeExprKind::Bool => GenericDefault::Builtin(BuiltinTy::Bool),
            TypeExprKind::String => GenericDefault::Builtin(BuiltinTy::String),
            TypeExprKind::Unit => GenericDefault::Builtin(BuiltinTy::Unit),
            TypeExprKind::Any => GenericDefault::Builtin(BuiltinTy::Any),
            TypeExprKind::Ident(n)
                if n.name.as_str() != "_" && !generics.iter().any(|s| s.name.name == n.name) =>
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

fn map_key(scope: &FragmentScope, path: &[ustr::Ustr], name: ustr::Ustr) -> ustr::Ustr {
    bone_core::modules::def_key(scope.package_name.as_ref(), path, name.as_str())
}

/// Declaration facts available before lowering the bodies.
struct ModuleNominalFacts {
    /// Each nominal type's key and facts, in declaration order.
    types: Vec<(ustr::Ustr, NominalFacts)>,
    decorators: Vec<ustr::Ustr>,
}

fn module_nominal_facts(scope: &FragmentScope, rock_mod: &Ast) -> ModuleNominalFacts {
    let mut facts = ModuleNominalFacts {
        types: Vec::new(),
        decorators: Vec::new(),
    };
    let mut walk = |item: &Item, path: &[ustr::Ustr]| -> Result<(), CompilerError> {
        let key = |name: &ustr::Ustr| map_key(scope, path, *name);
        let (name, flavor, generics) = match &item.kind {
            ItemKind::Struct(s) => (&s.name.name, NominalFlavor::Struct, &s.generics),
            ItemKind::Enum(e) => (&e.name.name, NominalFlavor::Enum, &e.generics),
            ItemKind::Newtype(n) => (&n.name.name, NominalFlavor::Newtype, &n.generics),
            ItemKind::Decorator(d) => {
                facts.decorators.push(key(&d.name.name));
                return Ok(());
            }
            _ => return Ok(()),
        };
        facts.types.push((
            key(name),
            NominalFacts {
                flavor,
                arity: generics.len(),
                defaults: trailing_generic_defaults(rock_mod, generics),
            },
        ));
        Ok(())
    };
    for_each_def_item(&rock_mod.items, &scope.module_path, &mut walk)
        .expect("reading nominal facts rejects nothing");
    facts
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NominalFlavor {
    Struct,
    Enum,
    Newtype,
}

/// Declaration facts that affect lowering at use sites.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NominalFacts {
    pub flavor: NominalFlavor,
    pub arity: usize,
    pub defaults: Vec<bone_core::hir::GenericDefault>,
}

pub fn nominal_type_facts(
    scope: &FragmentScope,
    rock_mod: &Ast,
) -> FxHashMap<ustr::Ustr, NominalFacts> {
    module_nominal_facts(scope, rock_mod)
        .types
        .into_iter()
        .collect()
}

fn expand_and_desugar_trees(cooked: &mut [Ast]) {
    let _span = tracing::info_span!("expand_and_desugar").entered();
    skull_parser::macro_expand::expand_macros(cooked);
    for ast in cooked.iter_mut() {
        skull_parser::desugar::desugar(ast);
    }
}

pub(crate) fn lower_modules(
    ws: &mut Workspace,
    rock_modules: &[Ast],
) -> Result<Vec<ModuleId>, CompilerError> {
    lower_modules_seeded(ws, rock_modules, FxHashMap::default())
}

/// Type-arena entries a module's next lowering starts from.
#[derive(Debug)]
pub struct TypeArenaSeed {
    pub arena: Arena<TypeExpr>,
    pub var_high_water: u32,
}

/// [`lower_modules`], with the listed modules allocating after the seeded
/// entries of their type arena instead of from an empty one.
pub fn lower_modules_seeded(
    ws: &mut Workspace,
    rock_modules: &[Ast],
    mut seeds: FxHashMap<ModuleId, TypeArenaSeed>,
) -> Result<Vec<ModuleId>, CompilerError> {
    let gap_timer = Instant::now();
    let mut modules = rock_modules
        .iter()
        .map(|rock_mod| Module {
            file: rock_mod.file,
            package: rock_mod.package.as_ref().map(|tag| ustr(&tag.self_name)),
            logical_file: rock_mod
                .package
                .as_ref()
                .and_then(|tag| tag.logical_file(&rock_mod.file))
                .map_or(rock_mod.file, |file| ustr(&file)),
            identity_storage: rock_mod
                .package
                .as_ref()
                .map(|tag| tag.identity_storage.clone())
                .unwrap_or_default(),
            ..Default::default()
        })
        .collect_vec();

    for m in modules.iter_mut() {
        m.id = ws.intern_module(m.file);
        if let Some(seed) = seeds.remove(&m.id) {
            m.ty_arena_live_start = seed.arena.len() as u32;
            m.ty_arena = seed.arena;
            m.lower_var_high_water = seed.var_high_water;
            m.lower_var_floor = seed.var_high_water;
        }
    }

    // Resolve cross-module generic arity before lowering. Merge in module order so
    // the first declaration wins, and let a relowered module refresh its own facts.
    let facts = rock_modules
        .par_iter()
        .map(|rock_mod| module_nominal_facts(&ws.fragment_scopes[&rock_mod.file], rock_mod))
        .collect::<Vec<_>>();
    for (module, facts) in modules.iter().zip(facts) {
        let mid = module.id;
        for (key, mut facts) in facts.types {
            for default in &mut facts.defaults {
                if let bone_core::hir::GenericDefault::Named(name) = default {
                    let mut scope = ws.fragment_scopes[&module.file].clone();
                    if let Some(meta) = ws.defs.get(&key) {
                        scope.module_path = meta.module.clone();
                    }
                    if let Ok(resolved) = bone_lower::resolve::resolve_bare(ws, &scope, *name) {
                        *name = resolved.key;
                    }
                }
            }
            if ws
                .type_generics
                .get(&key)
                .is_none_or(|entry| entry.owner == mid)
            {
                ws.type_generics.insert(
                    key,
                    TypeGenerics {
                        owner: mid,
                        arity: facts.arity,
                        defaults: facts.defaults,
                    },
                );
            }
        }
        ws.decorator_names.extend(facts.decorators);
    }

    ws.log_time("nominal facts", gap_timer);
    let mut items = modules.iter_mut().zip(rock_modules).collect_vec();

    {
        let _span = tracing::info_span!("forward_decls").entered();
        let timer = Instant::now();

        items
            .par_iter_mut()
            .map(|(module, ast)| {
                pass_register_forward_decls(ast, module, &ws.fragment_scopes[&ast.file])
            })
            .collect::<Result<Vec<()>, CompilerError>>()?;

        ws.log_time("register decls", timer);
    }
    let gap_timer = Instant::now();

    // Validate non-function annotations after more specific forward-declaration errors.
    // Resolve their names so user decorators cannot enter builtin branches.
    for (module, ast) in &mut items {
        let mut ctx = LowerCtx::new(ws, module, ast, 0);
        let fragment_scope = &ws.fragment_scopes[&ast.file];
        for_each_def_item(
            &ast.items,
            &fragment_scope.module_path,
            &mut |ast_item, path| {
                if ast_item.annotations.is_empty() {
                    return Ok(());
                }
                let pos = match &ast_item.kind {
                    ItemKind::Function(_) => return Ok(()),
                    ItemKind::Struct(_) => AnnoPos::StructItem,
                    ItemKind::Enum(_) => AnnoPos::EnumItem,
                    ItemKind::Newtype(_) => AnnoPos::NewtypeItem,
                    ItemKind::Global(_) => AnnoPos::GlobalItem,
                    ItemKind::Decorator(_) => AnnoPos::DecoItem,
                    ItemKind::CompilerDirective(_)
                    | ItemKind::Comment(_)
                    | ItemKind::MacroInvocation { .. } => return Ok(()),
                    ItemKind::Use(_) | ItemKind::Module(_) => return Ok(()),
                };
                let mut item_scope = fragment_scope.clone();
                item_scope.module_path = path.to_vec();
                let lowered = lower_annotations_in_scope(&ctx, &ast_item.annotations, &item_scope)?;
                validate_annotations(pos, &lowered, &ws.decorator_names)?;
                if let ItemKind::Struct(def) = &ast_item.kind {
                    let name = map_key(fragment_scope, path, def.name.name);
                    let index = ctx.module.nominals.struct_id(name).unwrap().def;
                    ctx.module.structs[index].consume =
                        lowered.iter().any(|anno| anno.is_builtin("consume"));
                }
                Ok(())
            },
        )?;
        ctx.flush_scope_probes();
    }

    ws.log_time("annotations", gap_timer);
    {
        let _span = tracing::info_span!("name_reg").entered();
        let timer = Instant::now();

        for (module, _) in &mut items {
            // Keep the first declaration so its uses still lower. Record duplicates as
            // interface errors so the editor can show them with partial results.
            let module = &mut **module;
            for (&name, &nominal) in module.nominals.iter() {
                let std::collections::hash_map::Entry::Vacant(slot) = ws.nominals.entry(name)
                else {
                    let (kind, span) = match nominal {
                        Nominal::Struct(id) => ("struct", module.structs[id.def].span),
                        Nominal::Enum(id) => ("enum", module.enums[id.def].span),
                        Nominal::Newtype(id) => ("newtype", module.newtypes[id.def].span),
                    };
                    module.interface_errors.push(CompilerError::new_with_label(
                        span,
                        format!("duplicate {kind} `{name}` across modules"),
                    ));
                    continue;
                };
                slot.insert(nominal);
            }
        }

        ws.log_time("register names", timer);
    }

    let lower_field_timer = Instant::now();
    let ctxts = {
        let _span = tracing::info_span!("lower_fields").entered();

        items
            .into_par_iter()
            .map(|(module, ast)| {
                let first_var = module.lower_var_high_water;
                let mut ctx = LowerCtx::new(ws, module, ast, first_var);

                pass_lower_fields(&mut ctx)?;
                bone_lower::record_use_paths(&mut ctx);

                Ok::<_, CompilerError>(ctx)
            })
            .collect::<Result<Vec<_>, _>>()?
    };

    let lower_field_time = lower_field_timer.elapsed();

    {
        let _span = tracing::info_span!("lower_functions").entered();
        let timer = Instant::now();

        let modules = par_map_largest_first(
            ctxts,
            |c| c.ast.exprs.len(),
            |mut c| {
                pass_lower_functions(&mut c)?;
                c.flush_scope_probes();

                c.module.lower_var_high_water = c._next_var_id;
                Ok::<_, CompilerError>(c.module)
            },
        )
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;

        for module in modules.iter() {
            for (name, fns) in module.functions_by_name.iter() {
                ws.functions_by_name
                    .entry(*name)
                    .or_default()
                    .extend(fns.iter().copied());
            }
        }

        ws.phase_timings.push(("lower fields", lower_field_time));
        ws.log_time("lower functions", timer);
    }

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
                if ws.functions_by_name.contains_key(&g.name) {
                    dup_errs.push(CompilerError::new_with_label(
                        g.span,
                        format!("global `{}` has the same name as a function", g.name),
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

    let gap_timer = Instant::now();
    ws.insert_modules(modules);
    ws.log_time("insert modules", gap_timer);

    check_duplicate_definitions(ws, &ids);
    let gap_timer = Instant::now();
    check_embed_site_legality(ws, &ids);
    crate::value_recursion::check_value_recursion(ws, &ids);
    ws.log_time("embed+value recursion", gap_timer);
    check_frame_ref_storage(ws, &ids);
    crate::identity_storage::check_identity_storage(ws, &ids);

    Ok(ids)
}

/// Overloads with the same inputs and backend conflict even if returns or parameter names differ.
fn check_duplicate_definitions(ws: &mut Workspace, fresh: &[ModuleId]) {
    let same = |a: (ModuleId, Idx<crate::hir::Function>),
                b: (ModuleId, Idx<crate::hir::Function>)| {
        let (fa, fb) = (
            &ws.module(a.0).functions[a.1],
            &ws.module(b.0).functions[b.1],
        );
        ws.param_signatures[a.0.index()][a.1] == ws.param_signatures[b.0.index()][b.1]
            && fa.backend == fb.backend
            && fa.is_variadic == fb.is_variadic
            && fa.generics.len() == fb.generics.len()
            && fa.generic_bounds.len() == fb.generic_bounds.len()
            && fa
                .generic_bounds
                .iter()
                .zip(&fb.generic_bounds)
                .all(|(x, y)| match (x, y) {
                    (None, None) => true,
                    (Some(x), Some(y)) => ws.render(None, *x) == ws.render(None, *y),
                    _ => false,
                })
    };
    let mut errs: Vec<(ModuleId, CompilerError)> = Vec::new();
    for fns in ws.functions_by_name.values() {
        for (j, &later) in fns.iter().enumerate().skip(1) {
            if !fresh.contains(&later.0) {
                continue;
            }
            let Some(&first) = fns[..j].iter().find(|&&first| same(first, later)) else {
                continue;
            };
            let f = &ws.module(later.0).functions[later.1];
            let at = ws.module(first.0).functions[first.1].name_span.start();
            let bare = f.name.rsplit("::").next().unwrap_or(&f.name);
            let params = ws.param_signatures[later.0.index()][later.1].replace('\u{1}', ", ");
            errs.push((
                later.0,
                CompilerError::new_with_label(
                    f.name_span,
                    format!(
                        "duplicate definition of `{bare}({params})`: already defined at {}:{}",
                        at.file,
                        at.line + 1
                    ),
                ),
            ));
        }
    }
    // Kept modules precede fresh ones so a duplicate reports at the fresh definition.
    let mut decorators = FxHashMap::default();
    let kept = ws.source_modules().filter(|m| !fresh.contains(&m.id));
    for m in kept.chain(fresh.iter().map(|&mid| ws.module(mid))) {
        for (_, d) in m.decorators.iter() {
            match decorators.entry(d.def_key) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(d.span);
                }
                std::collections::hash_map::Entry::Occupied(first) if fresh.contains(&m.id) => {
                    let at = first.get().start();
                    errs.push((
                        m.id,
                        CompilerError::new_with_label(
                            d.span,
                            format!(
                                "duplicate definition of decorator `{}`: already defined at {}:{}",
                                d.name,
                                at.file,
                                at.line + 1
                            ),
                        ),
                    ));
                }
                std::collections::hash_map::Entry::Occupied(_) => {}
            }
        }
    }
    for (mid, e) in errs {
        ws.module_mut(mid).interface_errors.push(e);
    }
}

fn check_frame_ref_storage(ws: &mut Workspace, fresh: &[ModuleId]) {
    let per_module: Vec<Vec<(ModuleId, CompilerError)>> = fresh
        .par_iter()
        .map(|&mid| {
            let module = ws.module(mid);
            let mut errs = Vec::new();
            let mut report = |span: Span, message: String| {
                errs.push((module.id, CompilerError::new_with_label(span, message)));
            };
            let stored = |what: &str, name: Ustr| {
                format!(
                    "{what} `{name}` cannot store a frame reference; store an `Entity<T>` or a value instead"
                )
            };
            for (_, def) in module.structs.iter() {
                for field in &def.fields {
                    if ws.type_contains_ref(None, field.ty) {
                        report(field.span, stored("field", field.name));
                    }
                }
            }
            for (_, def) in module.enums.iter() {
                for variant in &def.variants {
                    match &variant.payload {
                        VariantPayload::Unit => {}
                        VariantPayload::Tuple(types) => {
                            if types.iter().any(|ty| ws.type_contains_ref(None, *ty)) {
                                report(variant.name_span, stored("enum variant", variant.name));
                            }
                        }
                        VariantPayload::Record(fields) => {
                            for field in fields {
                                if ws.type_contains_ref(None, field.ty) {
                                    report(field.span, stored("enum field", field.name));
                                }
                            }
                        }
                    }
                }
            }
            for (_, def) in module.newtypes.iter() {
                if ws.type_contains_ref(None, def.wrapped) {
                    report(
                        def.span,
                        format!("newtype `{}` cannot store a frame reference", def.name),
                    );
                }
            }
            for (_, global) in module.globals.iter() {
                if ws.type_contains_ref(None, global.ty) {
                    report(global.span, stored("global", global.name));
                }
            }
            for (_, function) in module.functions.iter() {
                if !function.is_extern && function.inline_body.is_none() {
                    continue;
                }
                let carries_ref = function
                    .param_decls
                    .iter()
                    .any(|param| ws.type_contains_ref(None, param.ty))
                    || function
                        .return_ty
                        .is_some_and(|ty| ws.type_contains_ref(None, ty));
                if carries_ref {
                    report(
                        function.span,
                        format!(
                            "foreign or inline function `{}` cannot carry a frame reference across an opaque boundary",
                            function.name
                        ),
                    );
                }
            }
            errs
        })
        .collect();
    for (mid, err) in per_module.into_iter().flatten() {
        ws.module_mut(mid).interface_errors.push(err);
    }
}

enum EmbedTarget {
    Struct(StructId),
    NotAStruct(&'static str),
}

fn embed_target(ws: &Workspace, ty: TyId) -> EmbedTarget {
    let TyId::Static(mid, mut idx) = ty else {
        return EmbedTarget::NotAStruct("a body-local type");
    };
    let arena = &ws.module(mid).ty_arena;
    if let TypeExpr::Nullable(inner) = arena[idx] {
        idx = inner;
    }
    match arena[idx] {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(n),
            ..
        } => match ws.nominals.struct_id(n) {
            Some(sid) => EmbedTarget::Struct(sid),
            None => EmbedTarget::NotAStruct("a non-struct nominal type"),
        },
        TypeExpr::App {
            ctor: TypeCtor::Array,
            ..
        }
        | TypeExpr::App {
            ctor: TypeCtor::Map,
            ..
        } => EmbedTarget::NotAStruct("a collection"),
        TypeExpr::App {
            ctor: TypeCtor::Ref,
            ..
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

/// Embeds require known struct fields and an acyclic graph for finite storage.
/// Record only fresh interface errors so incremental runs don't duplicate them.
fn check_embed_site_legality(ws: &mut Workspace, fresh: &[ModuleId]) {
    let mut errs: Vec<(ModuleId, CompilerError)> = Vec::new();
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

    // Report cycles at every affected field, independent of traversal order.
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

pub(crate) fn infer_modules(ws: &mut Workspace, ids: &[ModuleId]) -> Result<(), CompilerError> {
    {
        let _span = tracing::info_span!("infer_functions").entered();
        let timer = Instant::now();

        let inference_results = par_map_largest_first(
            ids.to_vec(),
            |id| {
                ws.module(*id)
                    .bodies
                    .values()
                    .map(|body| body.expr_arena.len())
                    .sum()
            },
            |id| {
                let _m = tracing::info_span!("check_module", module = %id.0).entered();
                Ok((id, check_module(ws, id, ws.module(id))?))
            },
        )
        .into_iter()
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
    let timer = Instant::now();
    crate::relocation::check_relocation(ws);
    ws.log_time("relocation", timer);
    let timer = Instant::now();
    crate::fn_identity::check_generic_fn_eq(ws);
    ws.log_time("generic fn eq", timer);

    Ok(ids)
}

fn compute_export_reachability(
    ws: &Workspace,
    pkg_id: bone_core::modules::PackageId,
) -> (FxHashSet<Ustr>, FxHashSet<Ustr>) {
    let mut reachable_modules: FxHashSet<Ustr> = FxHashSet::default();
    let mut reachable_defs: FxHashSet<Ustr> = FxHashSet::default();

    let Some(pkg) = ws.ns.package(pkg_id) else {
        return (reachable_modules, reachable_defs);
    };

    let root_key = Ustr::from("");
    if pkg.node(root_key).is_some() {
        reachable_modules.insert(root_key);
    }

    let mut queue = std::collections::VecDeque::new();
    if reachable_modules.contains(&root_key) {
        queue.push_back(root_key);
    }

    while let Some(mkey) = queue.pop_front() {
        if let Some(node) = pkg.node(mkey) {
            for &child_key in node.children.values() {
                if let Some(child_node) = pkg.node(child_key) {
                    if child_node.vis == Some(NsVisibility::Pub) {
                        if reachable_modules.insert(child_key) {
                            queue.push_back(child_key);
                        }
                    }
                }
            }
            for binding in node.exports.values().chain(node.glob_exports.iter()) {
                if binding.vis == NsVisibility::Pub {
                    let frag = FragmentScope {
                        package: pkg_id,
                        module_path: node.segments.clone(),
                        ..Default::default()
                    };
                    if let Ok((target_pkg, target_path)) =
                        bone_lower::resolve::resolve_path_to_module(ws, &frag, &binding.target)
                    {
                        if target_pkg == pkg_id {
                            let target_key = NsPackage::path_key(&target_path);
                            if reachable_modules.insert(target_key) {
                                queue.push_back(target_key);
                            }
                        }
                    } else if let Ok(resolved) =
                        bone_lower::resolve::resolve_path(ws, &frag, &binding.target)
                    {
                        reachable_defs.insert(resolved.key);
                    }
                }
            }
        }
    }

    (reachable_modules, reachable_defs)
}

fn is_type_publicly_accessible(
    ws: &Workspace,
    pkg_id: bone_core::modules::PackageId,
    type_name: Ustr,
    reachable_modules: &FxHashSet<Ustr>,
    reachable_defs: &FxHashSet<Ustr>,
) -> bool {
    let Some(meta) = ws.defs.get(&type_name) else {
        return true;
    };
    if meta.package != pkg_id {
        return matches!(meta.vis, NsVisibility::Pub);
    }
    if reachable_defs.contains(&type_name) {
        return true;
    }
    if matches!(meta.vis, NsVisibility::Pub)
        && ws.ns.package(pkg_id).is_some()
        && reachable_modules.contains(&meta.node)
    {
        return true;
    }
    false
}

/// One inaccessible type an exported definition's interface names.
struct ClosureRequirement {
    required: Ustr,
    reason: &'static str,
    span: Span,
}

type ExportReachability =
    FxHashMap<bone_core::modules::PackageId, (FxHashSet<Ustr>, FxHashSet<Ustr>)>;

/// Each package's modules and definitions reachable from its root.
fn export_reachability(ws: &Workspace) -> ExportReachability {
    let packages: Vec<bone_core::modules::PackageId> = ws.ns.packages.keys().copied().collect();
    packages
        .par_iter()
        .map(|&pkg_id| (pkg_id, compute_export_reachability(ws, pkg_id)))
        .collect()
}

fn api_closure_failure_of(
    ws: &Workspace,
    reachability: &ExportReachability,
    def_key: &Ustr,
    meta: &bone_core::modules::DefMeta,
) -> std::ops::ControlFlow<ClosureRequirement> {
    let pkg_id = meta.package;
    let Some((reachable_modules, reachable_defs)) = reachability.get(&pkg_id) else {
        return std::ops::ControlFlow::Continue(());
    };

    let module_reachable =
        ws.ns.package(pkg_id).is_some() && reachable_modules.contains(&meta.node);
    let is_exported = match meta.kind {
        DefKind::Callable => {
            // Check reachability before scanning members of every overload family.
            (reachable_defs.contains(def_key) || module_reachable)
                && ws.functions_by_name.get(def_key).is_some_and(|fns| {
                    fns.iter()
                        .any(|&(mid, fid)| ws.callable_member_is_public(*def_key, mid, fid))
                })
        }
        _ => {
            reachable_defs.contains(def_key)
                || (matches!(meta.vis, NsVisibility::Pub) && module_reachable)
        }
    };

    let need = |ty: TyId, span: Span, reason: &'static str| {
        let mut failure = None;
        ws.type_any(None, ty, &mut |node| {
            if let TypeExpr::App {
                ctor: TypeCtor::Nominal(required),
                ..
            } = node
                && !is_type_publicly_accessible(
                    ws,
                    pkg_id,
                    required,
                    reachable_modules,
                    reachable_defs,
                )
            {
                failure = Some(ClosureRequirement {
                    required,
                    reason,
                    span,
                });
                return true;
            }
            false
        });
        failure.map_or(
            std::ops::ControlFlow::Continue(()),
            std::ops::ControlFlow::Break,
        )
    };

    if is_exported {
        match meta.kind {
            DefKind::Callable => {
                if let Some(fn_list) = ws.functions_by_name.get(def_key) {
                    for &(mid, fid) in fn_list {
                        if !ws.callable_member_is_public(*def_key, mid, fid) {
                            continue;
                        }
                        let f = &ws.module(mid).functions[fid];
                        for p in &f.param_decls {
                            need(p.ty, f.span, "public function")?;
                        }
                        if let Some(ret) = f.return_ty {
                            need(ret, f.span, "public function")?;
                        }
                        for bound_ty in f.generic_bounds.iter().flatten() {
                            need(*bound_ty, f.span, "generic bound")?;
                        }
                    }
                }
            }
            DefKind::Type => {
                if let Some(sid) = ws.nominals.struct_id(*def_key) {
                    let s = &ws.module(sid.module).structs[sid.def];
                    for f in &s.fields {
                        if f.read_vis == NsVisibility::Pub {
                            need(f.ty, f.span, "field type of exported record")?;
                        }
                    }
                } else if let Some(eid) = ws.nominals.enum_id(*def_key) {
                    let e = &ws.module(eid.module).enums[eid.def];
                    for v in &e.variants {
                        match &v.payload {
                            VariantPayload::Unit => {}
                            VariantPayload::Tuple(tys) => {
                                for ty in tys {
                                    need(*ty, v.name_span, "payload of exported enum")?;
                                }
                            }
                            VariantPayload::Record(fields) => {
                                for f in fields {
                                    need(f.ty, f.span, "payload of exported enum")?;
                                }
                            }
                        }
                    }
                } else if let Some(nid) = ws.nominals.newtype_id(*def_key) {
                    let nt = &ws.module(nid.module).newtypes[nid.def];
                    need(nt.wrapped, nt.span, "payload of exported newtype")?;
                }
            }
            DefKind::Global => {
                if let Some(&ty) = ws
                    .globals_by_name
                    .get(&meta.name)
                    .or_else(|| ws.globals_by_name.get(def_key))
                {
                    need(ty, meta.span, "type of exported global")?;
                }
            }
            _ => {}
        }
    }
    std::ops::ControlFlow::Continue(())
}

/// Public-API type-closure gate: reject the first type a public interface names that does
/// not support the interface's audience.
pub fn pass_validate_api_closure(ws: &Workspace) -> Result<(), CompilerError> {
    if ws.fragment_scopes.is_empty() || ws.ns.packages.is_empty() {
        return Ok(());
    }
    let reachability = export_reachability(ws);
    let unsatisfied = ws.defs.par_iter().find_map_first(|(def_key, meta)| {
        api_closure_failure_of(ws, &reachability, def_key, meta).break_value()
    });
    if let Some(r) = unsatisfied {
        let display_name = ws
            .defs
            .get(&r.required)
            .map(|m| m.name)
            .unwrap_or(r.required);
        comp_error!(
            r.span,
            "type `{}` in public API is not accessible (reason: {})",
            display_name,
            r.reason
        );
    }
    Ok(())
}

pub fn semantic_pass(rock_modules: &mut [Ast]) -> Result<Workspace, CompilerError> {
    semantic_pass_with_aliases(rock_modules, &bone_core::modules::NsAliases::default())
}

/// Lower an owned workspace with manifest dependency aliases and check API closure.
pub fn semantic_pass_with_aliases(
    rock_modules: &mut [Ast],
    aliases: &bone_core::modules::NsAliases,
) -> Result<Workspace, CompilerError> {
    let ws = semantic_pass_into(Workspace::default(), rock_modules, aliases)?;
    pass_validate_api_closure(&ws)?;
    Ok(ws)
}

/// Lower shared stdlib and user modules without closure checking or editor facts.
pub fn semantic_pass_without_closure_gate(
    modules: &mut ModuleSet,
    aliases: &bone_core::modules::NsAliases,
) -> Result<Workspace, CompilerError> {
    semantic_pass_owned(modules, aliases)
}

/// A check of the un-expanded modules whose diagnostics come before any the
/// semantic pass reports.
pub type PreExpansionGate<'g> = &'g (dyn Fn(&[&Ast]) -> Vec<CompilerError> + Sync);

#[derive(Debug)]
pub enum PassFailure {
    Gate(Vec<CompilerError>),
    Pass(CompilerError),
}

/// [`semantic_pass_without_closure_gate`] behind `gate`. The gate reads the
/// modules before expansion.
pub fn semantic_pass_after_gate(
    modules: &mut ModuleSet,
    aliases: &bone_core::modules::NsAliases,
    gate: PreExpansionGate,
) -> Result<Workspace, PassFailure> {
    let errors = gate(&modules.iter().collect_vec());
    if !errors.is_empty() {
        return Err(PassFailure::Gate(errors));
    }
    semantic_pass_owned(modules, aliases).map_err(PassFailure::Pass)
}

fn semantic_pass_owned(
    modules: &mut ModuleSet,
    aliases: &bone_core::modules::NsAliases,
) -> Result<Workspace, CompilerError> {
    let timer = Instant::now();
    let owned = modules.make_owned();
    let mut ws = Workspace {
        skip_session_facts: true,
        ..Workspace::default()
    };
    ws.log_time("own stdlib", timer);
    semantic_pass_into(ws, owned, aliases)
}

/// [`semantic_pass`] enforcing identity storage in unpackaged files: the bone
/// fixture harness.
pub fn semantic_pass_enforcing_identity_storage(
    rock_modules: &mut [Ast],
    aliases: &bone_core::modules::NsAliases,
) -> Result<Workspace, CompilerError> {
    let ws = Workspace {
        identity_storage_enforce_unpackaged: true,
        ..Workspace::default()
    };
    let ws = semantic_pass_into(ws, rock_modules, aliases)?;
    pass_validate_api_closure(&ws)?;
    Ok(ws)
}

pub fn semantic_pass_into(
    mut ws: Workspace,
    rock_modules: &mut [Ast],
    aliases: &bone_core::modules::NsAliases,
) -> Result<Workspace, CompilerError> {
    let _span = tracing::info_span!("bone_semantic_pass").entered();

    let timer = Instant::now();
    expand_and_desugar_trees(rock_modules);
    ws.log_time("macro_expand", timer);
    let timer = Instant::now();
    build_namespaces(&mut ws, &*rock_modules, aliases)?;
    ws.log_time("namespaces", timer);
    lower_and_infer(&mut ws, rock_modules)?;

    Ok(ws)
}

/// Walk every definition-bearing item, descending into inline mod blocks.
/// `path` is the logical module path within the file's package.
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

/// What one source file contributes to the namespace tree, read from its
/// un-expanded AST. Signatures, fields and bodies are not part of it, so an
/// edit to them keeps the contribution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FragmentContribution {
    pub file: ustr::Ustr,
    pub package: Option<skull_parser::PackageTag>,
    pub header: Option<skull_parser::ModuleHeader>,
    pub items: Vec<ScopeItem>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeItem {
    Module {
        name: ustr::Ustr,
        vis: Option<skull_parser::Visibility>,
        item_span: Span,
        block_span: Span,
        items: Vec<ScopeItem>,
    },
    Use(skull_parser::UseDecl),
    Def {
        name: ustr::Ustr,
        kind: DefKind,
        vis: Option<skull_parser::Visibility>,
        span: Span,
    },
}

pub fn contribute(rock_mod: &Ast) -> FragmentContribution {
    FragmentContribution {
        file: rock_mod.file,
        package: rock_mod.package.clone(),
        header: rock_mod.module_header.clone(),
        items: scope_items(&rock_mod.items),
    }
}

fn scope_items(items: &[Item]) -> Vec<ScopeItem> {
    items
        .iter()
        .filter_map(|item| match &item.kind {
            ItemKind::Module(m) => Some(ScopeItem::Module {
                name: m.name.name,
                vis: item.vis.clone(),
                item_span: item.span,
                block_span: m.span,
                items: scope_items(&m.items),
            }),
            ItemKind::Use(u) => Some(ScopeItem::Use(u.clone())),
            other => owned_name_and_kind(other).map(|(name, kind)| ScopeItem::Def {
                name,
                kind,
                vis: item.vis.clone(),
                span: item.span,
            }),
        })
        .collect()
}

/// Build definition keys and import scopes before lowering. Standalone files each own a package.
pub fn build_namespaces<'a>(
    ws: &mut Workspace,
    rock_modules: impl IntoIterator<Item = &'a Ast>,
    aliases: &bone_core::modules::NsAliases,
) -> Result<(), CompilerError> {
    let modules = rock_modules.into_iter().collect_vec();
    let contributions: Vec<_> = modules
        .par_iter()
        .map(|module| contribute(module))
        .collect();
    let built = assemble_namespaces(ws, &contributions.iter().collect_vec(), aliases);
    drop_in_background(contributions);
    built
}

/// [`build_namespaces`] from each file's [`FragmentContribution`], in module
/// order. It writes `ws.ns.packages`, `ws.defs` and `ws.fragment_scopes`,
/// which must start empty.
pub fn assemble_namespaces(
    ws: &mut Workspace,
    contributions: &[&FragmentContribution],
    aliases: &bone_core::modules::NsAliases,
) -> Result<(), CompilerError> {
    // Manifest aliases can refer to a package without a lowered fragment.
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
    let defs = contributions
        .iter()
        .flat_map(|contribution| &contribution.items)
        .filter(|item| matches!(item, ScopeItem::Def { .. }))
        .count();
    ws.defs.reserve(defs);
    ws.fragment_scopes.reserve(contributions.len());
    let mut next_standalone_id = aliases
        .packages
        .iter()
        .map(|p| p.id)
        .chain(
            contributions
                .iter()
                .filter_map(|c| c.package.as_ref().map(|p| p.id)),
        )
        .filter(|id| *id != STD_PACKAGE_ID)
        .max()
        .map_or(0, |id| id + 1);
    for contribution in contributions.iter() {
        if is_stdlib_module_file(contribution.file.as_str()) {
            if !ws.fragment_scopes.contains_key(&contribution.file) {
                register_stdlib_items(ws, contribution)?;
            }
            continue;
        }
        let header_span = contribution
            .header
            .as_ref()
            .map(|h| h.span)
            .unwrap_or_else(crate::prelude::Span::unknown);
        let tag = contribution.package.clone().unwrap_or_else(|| {
            let id = next_standalone_id;
            next_standalone_id += 1;
            skull_parser::PackageTag {
                id,
                self_name: format!(
                    "{}{}",
                    bone_core::modules::STANDALONE_PACKAGE_PREFIX,
                    contribution.file
                ),
                identity_storage: Default::default(),
                root: String::new(),
            }
        });
        let pkg_id = tag.id;
        if tag.self_name == STD_PACKAGE_NAME {
            return Err(CompilerError::new_with_label(
                header_span,
                "user packages cannot use the compiler-owned package name `std`".to_string(),
            ));
        }
        ws.ns.packages.entry(pkg_id).or_insert_with(|| NsPackage {
            id: pkg_id,
            self_name: ustr::Ustr::from(&tag.self_name),
            ..Default::default()
        });

        let module_path: Vec<ustr::Ustr> = contribution
            .header
            .as_ref()
            .map(|h| h.path.iter().map(|i| i.name).collect())
            .unwrap_or_default();
        if module_path
            .first()
            .is_some_and(|name| name.as_str() == STD_PACKAGE_NAME)
        {
            return Err(CompilerError::new_with_label(
                header_span,
                "package-root module name `std` is reserved for the compiler-owned standard library".to_string(),
            ));
        }

        let mut scope = FragmentScope {
            package: pkg_id,
            package_name: ustr::Ustr::from(&tag.self_name),
            module_path: module_path.clone(),
            ..Default::default()
        };

        let file = contribution.file;
        let mut errs: Vec<CompilerError> = Vec::new();
        if let Some(header) = &contribution.header
            && let Some(vis) = &header.vis
        {
            set_module_visibility(ws, pkg_id, &module_path, vis, header.span, || {
                module_path.iter().map(|name| name.as_str()).join("::")
            })?;
        }

        register_items(
            ws,
            pkg_id,
            contribution.file,
            &module_path,
            &contribution.items,
            &mut scope,
            &mut errs,
        );
        if !errs.is_empty() {
            errs.sort_by_key(|e| e.location.offset);
            return Err(errs.remove(0));
        }

        ws.fragment_scopes.insert(file, scope);
    }
    validate_v1_use_targets(ws, contributions)
}

fn ensure_namespace_node<'a>(
    ws: &'a mut Workspace,
    pkg_id: bone_core::modules::PackageId,
    path: &[ustr::Ustr],
    key: Ustr,
) -> &'a mut NsNode {
    let pkg = ws.ns.packages.get_mut(&pkg_id).expect("package ensured");
    pkg.nodes.entry(key).or_insert_with(|| NsNode {
        path_key: key,
        segments: path.to_vec(),
        ..Default::default()
    })
}

fn set_module_visibility(
    ws: &mut Workspace,
    pkg_id: bone_core::modules::PackageId,
    path: &[ustr::Ustr],
    vis: &skull_parser::Visibility,
    span: Span,
    name: impl FnOnce() -> String,
) -> Result<(), CompilerError> {
    let node = ensure_namespace_node(ws, pkg_id, path, NsPackage::path_key(path));
    let ns_vis = NsVisibility::from_parser(vis);
    if node
        .vis
        .as_ref()
        .is_some_and(|existing| *existing != ns_vis)
    {
        return Err(CompilerError::new_with_label(
            span,
            format!("conflicting visibility for module `{}`", name()),
        ));
    }
    node.vis = Some(ns_vis);
    Ok(())
}

/// Resolve imports after registration, from the fragment or exporting module.
fn validate_v1_use_targets(
    ws: &Workspace,
    contributions: &[&FragmentContribution],
) -> Result<(), CompilerError> {
    // Find the first error in module order even when checking in parallel.
    let first_error = contributions.par_iter().find_map_first(|contribution| {
        if is_stdlib_module_file(contribution.file.as_str()) {
            return None;
        }
        let frag = ws.fragment_scopes.get(&contribution.file)?;
        validate_use_items(ws, frag, &frag.module_path, &contribution.items).err()
    });
    first_error.map_or(Ok(()), Err)
}

fn validate_use_items(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &[ustr::Ustr],
    items: &[ScopeItem],
) -> Result<(), CompilerError> {
    for item in items {
        match item {
            ScopeItem::Module { name, items, .. } => {
                let mut child = path.to_vec();
                child.push(*name);
                validate_use_items(ws, frag, &child, items)?;
            }
            ScopeItem::Use(decl) => {
                let mut leaves = Vec::new();
                collect_use_leaves(&decl.tree, &[], false, &mut leaves);
                for (local, target) in leaves {
                    let glob = local.is_none();
                    let checked = if decl.vis.is_some() {
                        bone_lower::resolve::check_export_target(
                            ws,
                            frag.package,
                            path,
                            &target,
                            glob,
                        )
                    } else {
                        bone_lower::resolve::check_use_target(ws, frag, &target, glob)
                    };
                    checked.map_err(|message| {
                        CompilerError::new_with_label(
                            decl.span,
                            format!("invalid `use` target: {message}"),
                        )
                    })?;
                    if let Some(local) = local {
                        let exporter;
                        let site = if decl.vis.is_some() {
                            exporter = FragmentScope {
                                package: frag.package,
                                package_name: frag.package_name,
                                module_path: path.to_vec(),
                                ..Default::default()
                            };
                            &exporter
                        } else {
                            frag
                        };
                        bone_lower::resolve::check_use_binding(ws, site, local, &target)
                            .map_err(|message| CompilerError::new_with_label(decl.span, message))?;
                    }
                }
            }
            ScopeItem::Def { .. } => {}
        }
    }
    Ok(())
}

/// Stdlib definition keys remain bare so lowering and imports share one definition.
fn register_stdlib_items(
    ws: &mut Workspace,
    contribution: &FragmentContribution,
) -> Result<(), CompilerError> {
    let pkg = ws
        .ns
        .packages
        .entry(STD_PACKAGE_ID)
        .or_insert_with(|| NsPackage {
            id: STD_PACKAGE_ID,
            self_name: ustr(STD_PACKAGE_NAME),
            ..Default::default()
        });
    let root = pkg.nodes.entry(ustr("")).or_insert_with(|| NsNode {
        path_key: ustr(""),
        vis: Some(NsVisibility::Pub),
        ..Default::default()
    });
    root.vis = Some(NsVisibility::Pub);

    for item in &contribution.items {
        let ScopeItem::Def {
            name, kind, span, ..
        } = item
        else {
            continue;
        };
        let (name, kind) = (*name, *kind);
        let table = root.owned_table_mut(kind);
        if table.contains_key(&name) && kind != DefKind::Callable {
            return Err(CompilerError::new_with_label(
                *span,
                format!("duplicate standard-library export `{name}`"),
            ));
        }
        table.entry(name).or_insert(name);
    }

    for name in bone_core::modules::std_exported_intrinsic_types() {
        root.owned_types
            .entry(ustr(name))
            .or_insert_with(|| ustr(name));
    }
    ws.fragment_scopes.insert(
        contribution.file,
        FragmentScope {
            package: STD_PACKAGE_ID,
            package_name: ustr(STD_PACKAGE_NAME),
            ..Default::default()
        },
    );
    Ok(())
}

fn collect_use_leaves(
    tree: &skull_parser::UseTree,
    prefix: &[ustr::Ustr],
    from_root: bool,
    out: &mut Vec<(Option<ustr::Ustr>, UsePathRef)>,
) {
    let skull_parser::UseTree::Path {
        from_root: tree_root,
        segments,
        kind,
    } = tree;
    // Nested use paths inherit the declaration's root anchor.
    let path_root = *tree_root || from_root;
    let mut full = prefix.to_vec();
    full.extend(segments.iter().map(|segment| segment.name));
    // `p::m::{self}` / `{self as n}` binds the module `p::m` itself.
    let self_leaf =
        !prefix.is_empty() && segments.len() == 1 && segments[0].name.as_str() == "self";
    if self_leaf {
        full.pop();
    }
    let local = match kind {
        skull_parser::UseSuffix::Bind => {
            let Some(&local) = full.last() else { return };
            Some(local)
        }
        skull_parser::UseSuffix::BindAs(alias) => Some(alias.name),
        skull_parser::UseSuffix::Operator { op, alias } => {
            full.push(op.name);
            Some(alias.as_ref().map_or(op.name, |a| a.name))
        }
        skull_parser::UseSuffix::Glob => None,
        skull_parser::UseSuffix::Nested(children) => {
            for child in children {
                collect_use_leaves(child, &full, path_root, out);
            }
            return;
        }
    };
    out.push((
        local,
        UsePathRef {
            from_root: path_root,
            segments: full,
        },
    ));
}

fn source_module_context(scope: &FragmentScope, path: &[ustr::Ustr]) -> String {
    if path.is_empty()
        && scope
            .package_name
            .starts_with(bone_core::modules::STANDALONE_PACKAGE_PREFIX)
    {
        return String::new();
    }
    let name = if path.is_empty() {
        scope.package_name.to_string()
    } else {
        path.iter().map(|segment| segment.as_str()).join("::")
    };
    format!(" in module `{name}`")
}

fn register_items(
    ws: &mut Workspace,
    pkg_id: u32,
    file: ustr::Ustr,
    path: &[ustr::Ustr],
    items: &[ScopeItem],
    scope: &mut FragmentScope,
    errs: &mut Vec<CompilerError>,
) {
    // Header-only module paths still need ancestors for name resolution.
    for i in 1..=path.len() {
        let child = &path[..i];
        let parent = &path[..i - 1];
        let child_key = NsPackage::path_key(child);
        ensure_namespace_node(ws, pkg_id, child, child_key);
        ensure_namespace_node(ws, pkg_id, parent, NsPackage::path_key(parent))
            .children
            .entry(path[i - 1])
            .or_insert(child_key);
    }

    let node_key = NsPackage::path_key(path);
    let mut def_key_buf = scope.package_name.to_string();
    for seg in path {
        def_key_buf.push_str("::");
        def_key_buf.push_str(seg);
    }
    def_key_buf.push_str("::");
    let def_key_prefix = def_key_buf.len();

    for item in items {
        match item {
            ScopeItem::Module {
                name: block_name,
                vis: block_vis,
                item_span,
                items: block_items,
                ..
            } => {
                let mut child = path.to_vec();
                child.push(*block_name);
                if path.is_empty() && block_name.as_str() == STD_PACKAGE_NAME {
                    errs.push(CompilerError::new_with_label(
                        *item_span,
                        "the package-root module name `std` is reserved for the compiler-owned standard library".to_string(),
                    ));
                    continue;
                }
                {
                    let parent = ensure_namespace_node(ws, pkg_id, path, node_key);
                    if parent.owned(*block_name).is_some() {
                        errs.push(CompilerError::new_with_label(
                            *item_span,
                            format!(
                                "duplicate definition `{block_name}`{}",
                                source_module_context(scope, path)
                            ),
                        ));
                    } else {
                        parent
                            .children
                            .insert(*block_name, NsPackage::path_key(&child));
                    }
                }
                if let Some(vis) = block_vis {
                    if let Err(error) =
                        set_module_visibility(ws, pkg_id, &child, vis, *item_span, || {
                            block_name.to_string()
                        })
                    {
                        errs.push(error);
                        continue;
                    }
                }
                register_items(ws, pkg_id, file, &child, block_items, scope, errs);
            }
            ScopeItem::Use(u) => flatten_use_decl(ws, pkg_id, path, scope, u, errs),
            ScopeItem::Def {
                name: bare,
                kind,
                vis: item_vis,
                span: item_span,
            } => {
                let (bare, kind) = (*bare, *kind);
                if path.is_empty() {
                    let pkg = ws.ns.packages.get(&pkg_id).expect("package");
                    if bare.as_str() == STD_PACKAGE_NAME
                        || bare == pkg.self_name
                        || pkg.aliases.contains_key(&bare)
                    {
                        errs.push(CompilerError::new_with_label(
                            *item_span,
                            format!(
                                "root declaration `{bare}` collides with a package or dependency alias"
                            ),
                        ));
                        continue;
                    }
                }
                let node = ensure_namespace_node(ws, pkg_id, path, node_key);
                def_key_buf.truncate(def_key_prefix);
                def_key_buf.push_str(&bare);
                let def_key = ustr::Ustr::from(&def_key_buf);
                // The definition check reports duplicate decorators with their first location.
                if kind == DefKind::Decorator && node.owned_decorators.contains_key(&bare) {
                    continue;
                }
                let callable_overload =
                    kind == DefKind::Callable && node.owned_callables.contains_key(&bare);
                let taken = node.owned_types.contains_key(&bare)
                    || node.owned_globals.contains_key(&bare)
                    || (node.owned_callables.contains_key(&bare) && !callable_overload)
                    || node.owned_decorators.contains_key(&bare)
                    || node.children.contains_key(&bare);
                if taken {
                    errs.push(CompilerError::new_with_label(
                        *item_span,
                        format!(
                            "duplicate definition `{bare}`{}",
                            source_module_context(scope, path)
                        ),
                    ));
                    continue;
                }
                node.owned_table_mut(kind).insert(bare, def_key);
                let vis = NsVisibility::from_parser(
                    &item_vis
                        .clone()
                        .unwrap_or(skull_parser::Visibility::Private),
                );
                if kind == DefKind::Callable {
                    let members = node.callable_members.entry(bare).or_default();
                    let ordinal = members.iter().filter(|member| member.file == file).count();
                    members.push(bone_core::modules::CallableMember {
                        file,
                        ordinal,
                        vis: vis.clone(),
                    });
                }
                let meta = bone_core::modules::DefMeta {
                    package: pkg_id,
                    module: path.to_vec(),
                    node: node_key,
                    kind,
                    name: bare,
                    // Keep the first callable's location as stable family metadata.
                    vis,
                    span: *item_span,
                };
                if kind == DefKind::Callable {
                    ws.defs.entry(def_key).or_insert(meta);
                } else {
                    ws.defs.insert(def_key, meta);
                }
            }
        }
    }
}

fn flatten_use_decl(
    ws: &mut Workspace,
    pkg_id: u32,
    path: &[ustr::Ustr],
    scope: &mut FragmentScope,
    decl: &skull_parser::UseDecl,
    errs: &mut Vec<CompilerError>,
) {
    let mut leaves = Vec::new();
    collect_use_leaves(&decl.tree, &[], false, &mut leaves);
    for (local, target) in leaves {
        match (local, decl.vis.is_some()) {
            (local, true) => share_export(ws, pkg_id, path, local, target, decl, errs),
            (Some(local), false) => scope.named.entry(local).or_default().push(target),
            (None, false) => UsePathRef::insert_sorted(&mut scope.globs, target, |p| p),
        }
    }
}

fn share_export(
    ws: &mut Workspace,
    pkg_id: u32,
    path: &[ustr::Ustr],
    local: Option<ustr::Ustr>,
    target: UsePathRef,
    decl: &skull_parser::UseDecl,
    errs: &mut Vec<CompilerError>,
) {
    let vis = decl.vis.clone();
    let binding = bone_core::modules::ExportBinding {
        target,
        vis: NsVisibility::from_parser(&vis.unwrap_or(skull_parser::Visibility::Private)),
    };
    let node = ensure_namespace_node(ws, pkg_id, path, NsPackage::path_key(path));
    let Some(local) = local else {
        UsePathRef::insert_sorted(&mut node.glob_exports, binding, |b| &b.target);
        return;
    };
    if node.exports.insert(local, binding).is_some() {
        errs.push(CompilerError::new_with_label(
            decl.span,
            format!("duplicate export binding `{}`", local),
        ));
    }
}

fn owned_name_and_kind(item: &ItemKind) -> Option<(ustr::Ustr, DefKind)> {
    match item {
        ItemKind::Struct(s) => Some((s.name.name, DefKind::Type)),
        ItemKind::Enum(e) => Some((e.name.name, DefKind::Type)),
        ItemKind::Newtype(n) => Some((n.name.name, DefKind::Type)),
        ItemKind::Global(g) => Some((g.name.name, DefKind::Global)),
        // The macro registry owns discovery; expansion removes these before lowering.
        ItemKind::Function(f) if !f.is_macro => Some((f.name.name, DefKind::Callable)),
        ItemKind::Decorator(d) => Some((d.name.name, DefKind::Decorator)),
        _ => None,
    }
}

pub fn check_module_incremental(
    ws: &Workspace,
    mid: ModuleId,
    module: &Module,
    mut prev: InferenceResult,
) -> Result<InferenceResult, CompilerError> {
    let mut result = InferenceResult::default();
    let enforce_identity_storage = !crate::identity_storage::identity_storage_exempt(ws, module);

    let owners = module
        .functions
        .iter()
        .map(|(id, _)| BodyOwner::Function(id))
        .chain(
            module
                .decorators
                .iter()
                .map(|(id, _)| BodyOwner::Decorator(id)),
        )
        .chain(module.globals.iter().map(|(id, _)| BodyOwner::Global(id)))
        .chain(
            module
                .structs
                .iter()
                .map(|(id, _)| BodyOwner::StructDefaults(id)),
        );
    for owner in owners {
        let body = match owner {
            BodyOwner::Global(_) => &module.bodies[&owner],
            _ => match module.bodies.get(&owner) {
                Some(body) => body,
                None => continue,
            },
        };
        let function = if let BodyOwner::Function(fid) = owner {
            if reuse_body(&mut prev, &mut result, owner, body) {
                continue;
            }
            ws.reinferred_body_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some(&module.functions[fid])
        } else {
            None
        };
        match check_body(ws, BodyCtx { module, body }, mid, owner) {
            Some(Ok((mut inference, mut errors))) => {
                if enforce_identity_storage && let Some(function) = function {
                    errors.extend(
                        crate::identity_storage::identity_body_sites(
                            ws, function, body, &inference,
                        )
                        .into_iter()
                        .map(|site| CompilerError::new_with_label(site.span, site.message)),
                    );
                }
                errors.extend(crate::consume::check(ws, body, &inference, function));
                inference.shrink_to_fit();
                result.per_fn.insert(owner, inference);
                if !errors.is_empty() {
                    result.errors.insert(owner, errors);
                }
            }
            Some(Err(error)) => {
                result.errors.insert(owner, vec![error]);
            }
            None => {}
        }
    }

    result.per_fn.shrink_to_fit();
    Ok(result)
}

pub fn check_module(
    ws: &Workspace,
    mid: ModuleId,
    module: &Module,
) -> Result<InferenceResult, CompilerError> {
    check_module_incremental(ws, mid, module, InferenceResult::default())
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

        true
    } else {
        false
    }
}

pub(crate) fn pass_register_forward_decls(
    rock_module: &Ast,
    module: &mut Module,
    scope: &FragmentScope,
) -> Result<(), CompilerError> {
    for_each_def_item(&rock_module.items, &scope.module_path, &mut |item, path| {
        let def_key = |name: &ustr::Ustr| map_key(scope, path, *name);
        let mid = module.module_id();
        let (reg_name, name, nominal, kind) = if let ItemKind::Struct(s) = &item.kind {
            let reg_name = def_key(&s.name.name);
            let annotations = bone_lower::lower_annotations(rock_module, &item.annotations);
            let idx = module.structs.alloc(hir::StructDef {
                name: reg_name,
                name_span: s.name.span,
                is_value: s.is_value,
                is_nobox: s.is_nobox,
                consume: false,
                nomangle: annotations.iter().any(|anno| anno.is_builtin("nomangle")),
                span: item.span,
                generics: Vec::new(),
                fields: Vec::new(),
            });

            let id = StructId {
                module: mid,
                def: idx,
            };
            (reg_name, s.name.name, Nominal::Struct(id), "struct")
        } else if let ItemKind::Enum(e) = &item.kind {
            let reg_name = def_key(&e.name.name);
            let idx = module.enums.alloc(hir::EnumDef {
                name: reg_name,
                name_span: e.name.span,
                span: item.span,
                generics: Vec::new(),
                variants: Vec::new(),
            });

            let id = EnumId {
                module: mid,
                def: idx,
            };
            (reg_name, e.name.name, Nominal::Enum(id), "enum")
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

            let id = NewtypeId {
                module: mid,
                def: idx,
            };
            (reg_name, n.name.name, Nominal::Newtype(id), "newtype")
        } else {
            return Ok(());
        };
        match module.nominals.insert(reg_name, nominal) {
            Some(prev) if std::mem::discriminant(&prev) == std::mem::discriminant(&nominal) => {
                comp_error!(item.span, "duplicate {kind} definition: `{name}`")
            }
            Some(_) => comp_error!(item.span, "duplicate type definition: `{name}`"),
            None => Ok(()),
        }
    })?;
    Ok(())
}

pub(crate) fn pass_lower_fields(ctx: &mut LowerCtx) -> Result<(), CompilerError> {
    let a = ctx.ast;
    let scope = ctx.fragment_scope().clone();
    let reg_key = |name: &ustr::Ustr, path: &[ustr::Ustr]| map_key(&scope, path, *name);
    for_each_def_item(&a.items, &scope.module_path, &mut |item, path| {
        match &item.kind {
            ItemKind::Struct(s) => lower_struct_def(ctx, s, reg_key(&s.name.name, path))?,
            ItemKind::Enum(e) => lower_enum_def(ctx, e, reg_key(&e.name.name, path))?,
            ItemKind::Newtype(n) => lower_newtype_def(ctx, n, reg_key(&n.name.name, path))?,
            _ => {}
        }
        Ok(())
    })?;

    Ok(())
}

pub(crate) fn pass_lower_functions(ctx: &mut LowerCtx) -> Result<(), CompilerError> {
    let a = ctx.ast;
    let scope = ctx.fragment_scope().clone();
    let reg_key = |name: &ustr::Ustr, path: &[ustr::Ustr]| map_key(&scope, path, *name);
    for_each_def_item(&a.items, &scope.module_path, &mut |item, path| {
        match &item.kind {
            ItemKind::Function(f) => {
                if f.is_macro {
                    comp_error!(item.span, "macro `{}` survived expansion", f.name.name);
                }

                // Primitive declarations serve hard-coded operators. Newtype desugaring
                // creates the synthetic overloads needed to inherit those operators.
                let fn_name = f.name.name.as_str();
                let over_primitives = f.params.iter().all(|p| {
                    matches!(
                        a.types[p.ty].kind,
                        skull_parser::TypeExprKind::Int
                            | skull_parser::TypeExprKind::Uint
                            | skull_parser::TypeExprKind::Float
                            | skull_parser::TypeExprKind::F64
                            | skull_parser::TypeExprKind::String
                            | skull_parser::TypeExprKind::Bool
                            | skull_parser::TypeExprKind::Any
                    )
                });
                // The operator table excludes ordinary helpers such as __op_null_coalesce.
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

                let key = reg_key(&f.name.name, path);
                ctx.workspace
                    .relowered_body_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let fn_id =
                    lower_function_def(ctx, f, &item.annotations, item.span.offset_range().0, key)?;
                let mid = ctx.module.module_id();

                ctx.module
                    .functions_by_name
                    .entry(key)
                    .or_default()
                    .push((mid, fn_id));
            }

            ItemKind::Decorator(d) => {
                lower_decorator_def(ctx, d, &item.annotations, reg_key(&d.name.name, path))?;
            }

            ItemKind::Comment(_)
            | ItemKind::MacroInvocation { .. }
            | ItemKind::Struct(_)
            | ItemKind::Enum(_)
            | ItemKind::Newtype(_) => {}

            ItemKind::Use(_) | ItemKind::Module(_) => {}

            ItemKind::Global(g) => {
                lower_global(ctx, g, reg_key(&g.name.name, path))?;
            }

            ItemKind::CompilerDirective(_) => {
                comp_error!(item.span, "Unsupported top level item {:?}", item);
            }
        }
        Ok(())
    })?;
    Ok(())
}
