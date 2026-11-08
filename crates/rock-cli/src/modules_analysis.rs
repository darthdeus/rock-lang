//! The analysis `rock modules audit` and `rock modules migrate` share (spec
//! 15): one population rule, one checked workspace, and one walk listing every
//! reference a lowered body or signature resolved, site by site. The audit
//! aggregates the list into counts; the migrate planner reads individual
//! sites (what a consumer uses, what a fragment needs imported, which call a
//! candidate rebinds).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use bone_core::hir::{Callee, Expr, Function, Pattern, TyId, TypeCtor, TypeExpr};
use bone_core::prelude::{Idx, ModuleId};
use bone_core::workspace::{BodyInference, BoundNext, BoundTree};
use bone_semantic::inference::Workspace;
use rock_core::SourceFiles;
use rock_core::errors::CompilerError;
use rock_core::source_code::Span;
use rock_host::stick::StickConfig;
use rock_host::stick::graph::PackageGraph;
use ustr::Ustr;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PopMode {
    /// The `rock compile` set: each file is judged below its owning package root.
    Production,
    /// The whole-package set `rock test <dir>` builds.
    Test,
}

pub(crate) struct Population {
    pub mode: PopMode,
    pub discovered: SourceFiles,
    pub analyzed: SourceFiles,
}

/// The sources `project` compiles as. The default is the production population;
/// an explicit mode is used for migration/test-population analysis.
pub(crate) fn population(
    project: &Path,
    graph: &PackageGraph,
    mode: Option<PopMode>,
) -> Result<Population, String> {
    let spec = crate::unified_compile::SourceSpec::Directory(project.to_path_buf());
    let discovered =
        crate::unified_compile::discover_sources(&spec).map_err(|e| format!("{e:#}"))?;
    let root_path = &graph.package(graph.root()).root;
    let mode = mode.unwrap_or(PopMode::Production);
    let analyzed = match mode {
        PopMode::Production => discovered
            .production_only(root_path)
            .map_err(|error| error.to_string())?,
        PopMode::Test => discovered.clone(),
    };
    Ok(Population {
        mode,
        discovered,
        analyzed,
    })
}

/// Package id of every package-tagged file, keyed by its module file id, and
/// the ids of the files that carry no tag.
pub(crate) fn file_packages(sources: &SourceFiles) -> (HashMap<Ustr, u32>, Vec<String>) {
    let mut tagged = HashMap::new();
    let mut untagged = Vec::new();
    for f in &sources.files {
        let f = f.borrow();
        let id = f.file.to_id_path();
        match &f.package {
            Some(tag) => {
                tagged.insert(Ustr::from(id.as_str()), tag.id);
            }
            None => untagged.push(id),
        }
    }
    (tagged, untagged)
}

/// One compilation unit the package is analyzed in: the package itself or a
/// named consumer, each with its tests (the `rock test` population). A
/// consumer whose test population does not check is analyzed with its
/// `rock compile` population instead, and its test uses are unknown.
pub(crate) struct Ctx {
    pub root: PathBuf,
    pub mode: PopMode,
    pub graph: PackageGraph,
    pub sources: SourceFiles,
    /// The migrated package's id in this graph.
    pub p_id: u32,
    pub file_pkg: HashMap<Ustr, u32>,
}

impl Ctx {
    pub fn name(&self) -> &str {
        &self.graph.package(self.graph.root()).manifest_name
    }
}

pub(crate) fn load_ctx(root: &Path, p_root: &Path, mode: PopMode) -> Result<Ctx, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("`{}`: {e}", root.display()))?;
    let Some((config_path, config)) =
        StickConfig::find_from_dir(&root).map_err(|e| format!("{e:#}"))?
    else {
        return Err(format!("no stick.toml at or above `{}`", root.display()));
    };
    let manifest_dir = config_path.parent().unwrap_or(&config_path).to_path_buf();
    if manifest_dir.canonicalize().ok().as_deref() != Some(root.as_path()) {
        return Err(format!(
            "`{}` is not a package root (its nearest stick.toml is `{}`)",
            root.display(),
            config_path.display()
        ));
    }
    let channel = rock_host::stick::resolved_channel_for(&config);
    let graph = PackageGraph::build_with_channel(&manifest_dir, &config, &channel)
        .map_err(|e| format!("{e:#}"))?;
    let Some(p) = graph.packages().iter().find(|n| n.root == p_root) else {
        return Err(format!(
            "`{}` does not depend on the package being migrated (`{}`)",
            graph.package(graph.root()).manifest_name,
            p_root.display()
        ));
    };
    let p_id = p.id.0;
    let pop = population(&root, &graph, Some(mode))?;
    let (file_pkg, untagged) = file_packages(&pop.analyzed);
    if !untagged.is_empty() {
        return Err(format!(
            "{} analyzed file(s) carry no package tag: {}",
            untagged.len(),
            untagged.join(", ")
        ));
    }
    Ok(Ctx {
        root,
        graph,
        sources: pop.analyzed,
        p_id,
        file_pkg,
        mode,
    })
}

pub(crate) struct CheckFailure {
    pub stage: &'static str,
    pub errors: Vec<CompilerError>,
}

/// Parse-tree facts about one file, read before expansion and desugaring
/// rewrite the tree.
#[derive(Default, Clone, Copy)]
pub(crate) struct FileCensus {
    /// Legacy infix `expr :: T` ascriptions (the nodes a v1 conversion
    /// rewrites).
    pub legacy_ascriptions: usize,
    /// v1 `(expr : T)` ascriptions.
    pub paren_ascriptions: usize,
    /// `inline(<backend>) fn` bodies: raw backend source the compiler does
    /// not resolve.
    pub inline_fns: usize,
    /// `extern` declarations (C, Lua, JS, FFI).
    pub extern_fns: usize,
    /// Items emitted under their declared name (`@nomangle`).
    pub nomangle_items: usize,
}

pub(crate) struct Checked {
    pub ws: Workspace,
    /// Body errors; the workspace is kept beside them.
    pub errors: Vec<CompilerError>,
    /// By module file id; stdlib files included.
    pub census: HashMap<Ustr, FileCensus>,
}

/// Parse and check `sources` the way `rock compile` does, but keep the
/// workspace when bodies have errors (they come back beside it) and leave the
/// public-API type closure to the caller (`api_closure_requirements`).
pub(crate) fn check(sources: &SourceFiles) -> Result<Checked, CheckFailure> {
    let (mut modules, _entry) =
        crate::skull_backend::skull_assemble_project(sources).map_err(|errors| CheckFailure {
            stage: "parse",
            errors,
        })?;
    let census = modules.iter().map(|m| (m.file, census(m))).collect();
    let (ws, errors) =
        crate::bone_backend::check_modules_for_tools(&mut modules).map_err(|errors| {
            CheckFailure {
                stage: "semantic pass",
                errors,
            }
        })?;
    Ok(Checked { ws, errors, census })
}

fn census(ast: &skull_parser::Ast) -> FileCensus {
    let mut c = FileCensus::default();
    for (_, e) in ast.exprs.iter() {
        match e.kind {
            skull_parser::ExprKind::TypeAscription(..) => c.legacy_ascriptions += 1,
            skull_parser::ExprKind::ParenAscription(..) => c.paren_ascriptions += 1,
            _ => {}
        }
    }
    let mut items: Vec<&skull_parser::Item> = ast.items.iter().collect();
    while let Some(item) = items.pop() {
        if item
            .annotations
            .iter()
            .any(|a| a.name.name.as_str() == "nomangle")
        {
            c.nomangle_items += 1;
        }
        match &item.kind {
            skull_parser::ItemKind::Function(f) => match f.body {
                skull_parser::FunctionBody::Rock(_) => {}
                skull_parser::FunctionBody::Inline(..) => c.inline_fns += 1,
                _ => c.extern_fns += 1,
            },
            skull_parser::ItemKind::Module(m) => items.extend(m.items.iter()),
            _ => {}
        }
    }
    c
}

/// What a reference site is.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord, Hash)]
pub(crate) enum RefKind {
    /// A call inference bound to a function: named, UFCS, operator and
    /// compound-assignment calls.
    Call,
    /// A global, a function used as a value, a type used as a value, or a
    /// call through a fn-valued global.
    Value,
    /// A struct literal or a newtype constructor call.
    Construction,
    EnumVariant,
    /// A compiler-synthesized protocol call: a `for` loop's `iter`/`next`, or
    /// an iterable coerced to an `Iterator`-bounded parameter.
    Protocol,
    /// A nominal type named in a signature, field, annotation or cast.
    Type,
    /// An enum or record named in a match pattern.
    Pattern,
    /// A decorator applied to a function (`@name`).
    Annotation,
}

impl RefKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RefKind::Call => "calls",
            RefKind::Value => "values",
            RefKind::Construction => "constructions",
            RefKind::EnumVariant => "enum_variants",
            RefKind::Protocol => "protocol",
            RefKind::Type => "types",
            RefKind::Pattern => "patterns",
            RefKind::Annotation => "annotations",
        }
    }

    /// Compared per file rather than by site (`sites`).
    pub(crate) fn per_file(self) -> bool {
        matches!(self, RefKind::Type | RefKind::Pattern | RefKind::Annotation)
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Target {
    /// The exact function inference selected.
    Fn(ModuleId, Idx<Function>),
    /// A definition named by key; `modules` are the modules defining that
    /// key (none: a raw Lua global; several: a legacy family spanning files
    /// used as a value).
    Key { key: Ustr, modules: Vec<ModuleId> },
    /// A named call inference recorded no target for and nothing defines.
    Unrecorded(Ustr),
    /// A method-syntax call through a fn-typed field of struct `strukt`
    /// (defined in `module`), which wins over every function of the name.
    Field {
        module: ModuleId,
        strukt: Ustr,
        field: Ustr,
    },
}

/// The callee of a call written by name, for a repair that qualifies it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CalleeSite {
    pub name: Ustr,
    pub name_span: Span,
    /// Written as `recv.name(..)`.
    pub method: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Ref {
    pub from: ModuleId,
    /// The site, when lowering recorded one.
    pub span: Option<Span>,
    pub kind: RefKind,
    pub target: Target,
    pub callee: Option<CalleeSite>,
}

/// Every resolved reference in every non-builtin module of `ws`.
pub(crate) fn references(ws: &Workspace) -> Vec<Ref> {
    let mut global_modules: HashMap<Ustr, Vec<ModuleId>> = HashMap::new();
    for m in &ws.modules {
        for key in m.global_by_name.keys() {
            global_modules.entry(*key).or_default().push(m.id);
        }
    }
    let mut decorator_modules: HashMap<Ustr, Vec<ModuleId>> = HashMap::new();
    for m in &ws.modules {
        for (_, d) in m.decorators.iter() {
            decorator_modules.entry(d.def_key).or_default().push(m.id);
        }
    }
    let type_modules = |key: Ustr| -> Vec<ModuleId> {
        let mut out = Vec::new();
        if let Some(s) = ws.struct_by_name.get(&key) {
            out.push(s.module);
        }
        if let Some(e) = ws.enums_by_name.get(&key) {
            out.push(e.module);
        }
        if let Some(n) = ws.newtypes_by_name.get(&key) {
            out.push(n.module);
        }
        out
    };
    let nominal = |te: &TypeExpr| match te {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } => Some(*name),
        _ => None,
    };

    let mut out = Vec::new();
    for m in &ws.modules {
        if m.id == ModuleId::builtin() {
            continue;
        }
        for (idx, te) in m.ty_arena.iter() {
            if let Some(name) = nominal(te) {
                out.push(Ref {
                    from: m.id,
                    span: m.ty_spans.get(idx).copied(),
                    kind: RefKind::Type,
                    target: Target::Key {
                        key: name,
                        modules: type_modules(name),
                    },
                    callee: None,
                });
            }
        }
        // An application names its decorator by the identity lifecycle
        // discovery keys on; a name no decorator defines is compiler metadata.
        for (_, f) in m.functions.iter() {
            for a in &f.annotations {
                let key = a.resolved_name.unwrap_or(a.name.name);
                let Some(modules) = decorator_modules.get(&key) else {
                    continue;
                };
                out.push(Ref {
                    from: m.id,
                    span: Some(a.span),
                    kind: RefKind::Annotation,
                    target: Target::Key {
                        key,
                        modules: modules.clone(),
                    },
                    callee: None,
                });
            }
        }
        let inference = ws.inference(m.id);
        for (body_owner, body) in &m.bodies {
            let bi = inference.and_then(|inf| inf.per_fn.get(body_owner));
            for (idx, te) in body.lowered_tys.iter() {
                if let Some(name) = nominal(te) {
                    out.push(Ref {
                        from: m.id,
                        span: body.lowered_ty_spans.get(idx).copied(),
                        kind: RefKind::Type,
                        target: Target::Key {
                            key: name,
                            modules: type_modules(name),
                        },
                        callee: None,
                    });
                }
            }
            for (_, pat) in body.pattern_arena.iter() {
                let (name, span) = match pat {
                    Pattern::EnumVariant {
                        enum_name: Some(name),
                        enum_name_span,
                        ..
                    } => (*name, *enum_name_span),
                    Pattern::Record {
                        enum_name: Some(name),
                        enum_name_span,
                        ..
                    } => (*name, *enum_name_span),
                    Pattern::Record {
                        enum_name: None,
                        name: Some(name),
                        name_span,
                        ..
                    } => (*name, *name_span),
                    _ => continue,
                };
                out.push(Ref {
                    from: m.id,
                    span,
                    kind: RefKind::Pattern,
                    target: Target::Key {
                        key: name,
                        modules: type_modules(name),
                    },
                    callee: None,
                });
            }
            for (idx, expr) in body.expr_arena.iter() {
                let span = body.expr_spans.get(idx).copied();
                let site = match expr {
                    Expr::Call {
                        callee: Callee::Unresolved { name, name_span },
                        args,
                        ..
                    } => Some(CalleeSite {
                        name: *name,
                        name_span: *name_span,
                        method: !args.is_empty() && body.method_call_recvs.contains(&args.start()),
                    }),
                    _ => None,
                };
                // Every call inference bound, whatever its syntax (named, UFCS,
                // operator, compound assignment, a callee that is a value).
                let bound = bi
                    .and_then(|bi| bi.resolved_overload_calls.get(&idx))
                    .copied();
                if let Some(bi) = bi {
                    if let Some(&field) = bi.fn_field_calls.get(&idx) {
                        let target = fn_field_target(ws, bi, expr, field);
                        out.push(Ref {
                            from: m.id,
                            span,
                            kind: RefKind::Call,
                            target,
                            callee: site,
                        });
                        continue;
                    }
                }
                if let Some((mid, fid)) = bound {
                    out.push(Ref {
                        from: m.id,
                        span,
                        kind: RefKind::Call,
                        target: Target::Fn(mid, fid),
                        callee: site,
                    });
                }
                let (key, modules, kind) = match expr {
                    Expr::Call {
                        callee: Callee::Unresolved { name, .. },
                        ..
                    } => {
                        if bound.is_some() {
                            continue;
                        }
                        // Inference bound no function: a newtype constructor or
                        // a call through a fn-valued global, attributed like the
                        // definition it names.
                        let types = type_modules(*name);
                        let kind = if types.is_empty() {
                            RefKind::Value
                        } else {
                            RefKind::Construction
                        };
                        let mut mids = global_modules.get(name).cloned().unwrap_or_default();
                        mids.extend(types);
                        if mids.is_empty() {
                            out.push(Ref {
                                from: m.id,
                                span,
                                kind: RefKind::Call,
                                target: Target::Unrecorded(*name),
                                callee: site,
                            });
                            continue;
                        }
                        (*name, mids, kind)
                    }
                    Expr::Global(key) => {
                        let mut mids = global_modules.get(key).cloned().unwrap_or_default();
                        if let Some(fns) = ws.functions_by_name.get(key) {
                            mids.extend(fns.iter().map(|(mid, _)| *mid));
                        }
                        mids.extend(type_modules(*key));
                        (*key, mids, RefKind::Value)
                    }
                    Expr::StructLiteral { name, .. } => {
                        (*name, type_modules(*name), RefKind::Construction)
                    }
                    Expr::EnumVariant { enum_name, .. } => {
                        (*enum_name, type_modules(*enum_name), RefKind::EnumVariant)
                    }
                    _ => continue,
                };
                out.push(Ref {
                    from: m.id,
                    span,
                    kind,
                    target: Target::Key { key, modules },
                    callee: site,
                });
            }
            let Some(bi) = bi else { continue };
            let mut protocol = |idx: Idx<Expr>, func: (ModuleId, Idx<Function>)| {
                out.push(Ref {
                    from: m.id,
                    span: body.expr_spans.get(idx).copied(),
                    kind: RefKind::Protocol,
                    target: Target::Fn(func.0, func.1),
                    callee: None,
                });
            };
            for (&idx, info) in &bi.for_iter_protocol {
                if let Some(iter) = &info.iter {
                    protocol(idx, iter.func);
                }
                let mut funcs = Vec::new();
                bound_next_funcs(&info.next, &mut funcs);
                for f in funcs {
                    protocol(idx, f);
                }
            }
            for (&idx, witness) in &bi.implicit_iter_args {
                protocol(idx, witness.func);
            }
        }
    }
    out
}

/// The target of a call inference routed through the receiver's fn-typed
/// `field` (`BodyInference::fn_field_calls`, which skull-mir lowers from). The
/// receiver's type only names the field's owner; a receiver whose owner cannot
/// be read is reported as an unrecorded call rather than attributed elsewhere.
fn fn_field_target(ws: &Workspace, bi: &BodyInference, expr: &Expr, field: Ustr) -> Target {
    let recv = match expr {
        Expr::Call { args, .. } if !args.is_empty() => Some(args.start()),
        Expr::SafeNavCall { receiver, .. } => Some(*receiver),
        _ => None,
    };
    let owner = recv
        .and_then(|recv| resolved_ty(ws, bi, *bi.expr_types.get(recv)?))
        .and_then(|ty| match ty {
            TypeExpr::App {
                ctor: TypeCtor::Nominal(strukt),
                ..
            } => Some((ws.struct_by_name.get(&strukt)?.module, strukt)),
            _ => None,
        });
    match owner {
        Some((module, strukt)) => Target::Field {
            module,
            strukt,
            field,
        },
        None => Target::Unrecorded(field),
    }
}

/// `ty` with inference variables and nullability peeled.
fn resolved_ty(ws: &Workspace, bi: &BodyInference, ty: TyId) -> Option<TypeExpr> {
    let node = match ty {
        TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
        TyId::Local(idx) => bi.ty_arena[idx],
    };
    match node {
        TypeExpr::Infer(var, _) => resolved_ty(ws, bi, bi.substitutions.get(var)?),
        TypeExpr::Nullable(inner) => resolved_ty(ws, bi, ty.with_idx(inner)),
        other => Some(other),
    }
}

fn bound_next_funcs(next: &BoundNext, out: &mut Vec<(ModuleId, Idx<Function>)>) {
    if let BoundNext::Concrete { func, sub, .. } = next {
        out.push(*func);
        bound_tree_funcs(sub, out);
    }
}

fn bound_tree_funcs(tree: &BoundTree, out: &mut Vec<(ModuleId, Idx<Function>)>) {
    for next in tree.next.values() {
        bound_next_funcs(next, out);
    }
}

/// A key's last `::` segment: the declared name of a v1 DefKey, the key itself
/// for a legacy one.
pub(crate) fn declared_name(key: &str) -> &str {
    key.rsplit("::").next().unwrap_or(key)
}

/// Every contextual-ascription node (`expr :: T` in legacy, `(expr : T)` in
/// v1) in one parsed file: the node, its operand and its type, as byte ranges.
pub(crate) struct Ascription {
    pub node: (usize, usize),
    pub operand: (usize, usize),
    pub ty: (usize, usize),
}

pub(crate) fn ascriptions(ast: &skull_parser::Ast) -> Vec<Ascription> {
    let mut out: Vec<Ascription> = ast
        .exprs
        .iter()
        .filter_map(|(_, e)| match &e.kind {
            skull_parser::ExprKind::TypeAscription(operand, ty) => Some(Ascription {
                node: e.span.offset_range,
                operand: ast.exprs[*operand].span.offset_range,
                ty: ast.types[*ty].span.offset_range,
            }),
            _ => None,
        })
        .collect();
    out.sort_by_key(|a| a.node);
    out
}

/// One `inline(lua)` body for the raw-ABI census: its parameter names and
/// source, wrapped as the Lua emitter wraps it (a value-returning body with no
/// `return` is an expression).
pub(crate) struct LuaBody {
    pub params: Vec<Ustr>,
    pub returns_value: bool,
    pub src: Ustr,
}

/// References in an inline Lua body. Free globals and resolved `_G` fields
/// are distinct from conservative candidates in unresolved reflection.
#[derive(Debug, Default)]
pub(crate) struct LuaRefs {
    /// Free globals the body (or a function nested in it) reads or writes.
    pub globals: Vec<String>,
    /// Literal field accesses on the global environment, including aliases.
    pub via_g: Vec<String>,
    /// String candidates retained when environment flow cannot be resolved.
    pub conservative: Vec<String>,
    /// Coverage limits; dynamic names are not proven safe by this census.
    pub unresolved: Vec<String>,
    /// The body does not compile as Lua; its references are unknown.
    pub error: Option<String>,
}

/// Compile bodies with the toolchain's LuaJIT and track environment registers
/// over bytecode control flow. Unresolved reflection keeps the previous string
/// census, explicitly conservative. One process serves all bodies.
pub(crate) fn lua_references(bodies: &[LuaBody]) -> Result<Vec<LuaRefs>, String> {
    if bodies.is_empty() {
        return Ok(Vec::new());
    }
    let mut script = String::from("local bodies = {\n");
    for b in bodies {
        let src = if b.returns_value && !b.src.as_str().contains("return") {
            format!("return {}", b.src.as_str().trim())
        } else {
            b.src.to_string()
        };
        let params = b
            .params
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        script.push_str(&format!(
            "  {},\n",
            lua_quote(&format!("return function({params})\n{src}\nend"))
        ));
    }
    script.push_str("}\n");
    script.push_str(include_str!("modules_lua_references.lua"));
    let run = crate::compile_cmd::run_lua_code_with_timeout(
        &script,
        Some(std::time::Duration::from_secs(60)),
    )
    .map_err(|e| format!("the raw-ABI census could not run LuaJIT: {}", e.message))?;
    if !run.success {
        return Err(format!(
            "the raw-ABI census failed in LuaJIT: {}",
            run.stderr.trim()
        ));
    }
    let mut refs: Vec<LuaRefs> = bodies.iter().map(|_| LuaRefs::default()).collect();
    for line in run.stdout.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(tag), Some(index), Some(rest)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(format!("unexpected raw-ABI census output: {line}"));
        };
        let i = index
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .filter(|&n| n < bodies.len())
            .ok_or_else(|| format!("unexpected raw-ABI census output: {line}"))?;
        match tag {
            "E" => {
                let message = refs[i].error.get_or_insert_with(String::new);
                if !message.is_empty() {
                    message.push_str("; ");
                }
                message.push_str(rest);
            }
            "G" => refs[i].globals.push(rest.to_string()),
            "K" => refs[i].via_g.push(rest.to_string()),
            "C" => refs[i].conservative.push(rest.to_string()),
            "U" => refs[i].unresolved.push(rest.to_string()),
            _ => return Err(format!("unexpected raw-ABI census output: {line}")),
        }
    }
    for r in &mut refs {
        r.globals.sort();
        r.globals.dedup();
        r.via_g.sort();
        r.via_g.dedup();
        r.conservative.sort();
        r.conservative.dedup();
        r.unresolved.sort();
        r.unresolved.dedup();
    }
    Ok(refs)
}

/// `text` as a Lua string literal.
fn lua_quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c.is_ascii_control() => out.push_str(&format!("\\{:03}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// --- edits and site comparison (migrate and fix --imports) -------------------------------------------------------------------

/// One span edit: replace `range` (empty for an insertion) with `text`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub range: (usize, usize),
    pub text: String,
    /// An insertion that ends the text before it (an ascription's `)`), so a
    /// span ending there contains it. Any other insertion begins the text
    /// after it (`pub `, a qualifier, a `use` line).
    pub closes: bool,
}

impl Edit {
    pub fn new(range: (usize, usize), text: impl Into<String>) -> Edit {
        Edit {
            range,
            text: text.into(),
            closes: false,
        }
    }
}

/// Apply non-overlapping edits. Overlap is refused.
pub fn apply_edits(text: &str, edits: &[Edit]) -> std::result::Result<String, String> {
    let mut sorted: Vec<&Edit> = edits.iter().collect();
    sorted.sort_by_key(|e| e.range);
    let mut out = String::with_capacity(text.len() + 64);
    let mut at = 0;
    for e in sorted {
        if e.range.0 < at {
            return Err(format!("overlapping edits at byte {}", e.range.0));
        }
        if e.range.1 > text.len()
            || !text.is_char_boundary(e.range.0)
            || !text.is_char_boundary(e.range.1)
        {
            return Err(format!("edit range {:?} is outside the file", e.range));
        }
        out.push_str(&text[at..e.range.0]);
        out.push_str(&e.text);
        at = e.range.1;
    }
    out.push_str(&text[at..]);
    Ok(out)
}

/// Where a baseline byte offset lands after `edits`. An offset AT an insertion
/// stays before it (a qualified call starts where its name started) unless the
/// insertion `closes` the text before it (an expression ending in a rewritten
/// ascription ends after its `)`); a replaced range collapses onto its start.
pub(crate) fn map_offset(edits: &[Edit], off: usize) -> usize {
    let mut shift: isize = 0;
    for e in edits {
        let (a, b) = e.range;
        if a == b {
            if off > a || (off == a && e.closes) {
                shift += e.text.len() as isize;
            }
        } else if off >= b {
            shift += e.text.len() as isize - (b - a) as isize;
        } else if off > a {
            shift -= (off - a) as isize;
        }
    }
    (off as isize + shift) as usize
}

// --- identities used to compare a baseline with a candidate -------------------

/// A resolved target, spelled so a legacy key and its v1 DefKey agree.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TargetId {
    Fn {
        file: Ustr,
        name: String,
        ordinal: usize,
    },
    Def {
        file: Ustr,
        name: String,
    },
    Family {
        files: Vec<Ustr>,
        name: String,
    },
    Unrecorded(String),
    Field {
        file: Ustr,
        strukt: String,
        field: String,
    },
}

pub(crate) fn target_id(ws: &Workspace, t: &Target) -> TargetId {
    match t {
        Target::Fn(mid, fid) => {
            let m = ws.module(*mid);
            let name = declared_name(m.functions[*fid].name.as_str()).to_string();
            let ordinal = m
                .functions
                .iter()
                .filter(|(i, f)| {
                    u32::from(i.into_raw()) < u32::from(fid.into_raw())
                        && declared_name(f.name.as_str()) == name
                })
                .count();
            TargetId::Fn {
                file: m.file,
                name,
                ordinal,
            }
        }
        Target::Key { key, modules } => {
            let name = declared_name(key.as_str()).to_string();
            let mut files: Vec<Ustr> = modules.iter().map(|m| ws.module(*m).file).collect();
            files.sort();
            files.dedup();
            match files.as_slice() {
                [one] => TargetId::Def { file: *one, name },
                _ => TargetId::Family { files, name },
            }
        }
        Target::Unrecorded(name) => TargetId::Unrecorded(name.to_string()),
        Target::Field {
            module,
            strukt,
            field,
        } => TargetId::Field {
            file: ws.module(*module).file,
            strukt: declared_name(strukt.as_str()).to_string(),
            field: field.to_string(),
        },
    }
}

/// A reference site, in the coordinates of the text it was checked in.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SiteKey {
    pub file: Ustr,
    pub start: usize,
    pub end: usize,
    pub kind: RefKind,
}

pub(crate) type Sites = BTreeMap<SiteKey, Vec<TargetId>>;

/// Every package-file reference site; `edits` maps a baseline site into
/// candidate coordinates (empty for a candidate). Types, patterns and
/// decorator applications are compared per file as a multiset rather than by
/// site: a v1 signature type records no span (`span_recordable` tests the
/// spelled name against maps keyed by DefKey), an application leads an item
/// that `pub ` is inserted before, and neither name can be overloaded, so a
/// per-file count is as strict.
pub(crate) fn sites(
    ws: &Workspace,
    refs: &[Ref],
    file_pkg: &HashMap<Ustr, u32>,
    edits: &HashMap<Ustr, Vec<Edit>>,
) -> Sites {
    let mut out: Sites = BTreeMap::new();
    for r in refs {
        let file = ws.module(r.from).file;
        if !file_pkg.contains_key(&file) {
            continue;
        }
        if r.kind.per_file() {
            out.entry(SiteKey {
                file,
                start: 0,
                end: 0,
                kind: r.kind,
            })
            .or_default()
            .push(target_id(ws, &r.target));
            continue;
        }
        let Some(span) = r.span else { continue };
        if span.file != file {
            continue;
        }
        let (start, end) = match edits.get(&file) {
            Some(e) => (
                map_offset(e, span.offset_range.0),
                map_offset(e, span.offset_range.1),
            ),
            None => span.offset_range,
        };
        out.entry(SiteKey {
            file,
            start,
            end,
            kind: r.kind,
        })
        .or_default()
        .push(target_id(ws, &r.target));
    }
    for v in out.values_mut() {
        v.sort();
    }
    out
}

pub(crate) fn map_sites(base: &Sites, edits: &HashMap<Ustr, Vec<Edit>>) -> Sites {
    base.iter()
        .map(|(k, v)| {
            let key = match edits.get(&k.file) {
                Some(_) if k.kind.per_file() => k.clone(),
                Some(e) => SiteKey {
                    file: k.file,
                    start: map_offset(e, k.start),
                    end: map_offset(e, k.end),
                    kind: k.kind,
                },
                None => k.clone(),
            };
            (key, v.clone())
        })
        .collect()
}

pub(crate) fn describe_change(key: &SiteKey, before: &[TargetId], after: &[TargetId]) -> String {
    let show = |v: &[TargetId]| {
        if v.is_empty() {
            return "nothing".to_string();
        }
        v.iter()
            .map(|t| match t {
                TargetId::Fn {
                    file,
                    name,
                    ordinal,
                } => format!("fn {name}#{ordinal} in {}", short_path(file)),
                TargetId::Def { file, name } => format!("{name} in {}", short_path(file)),
                TargetId::Family { files, name } => {
                    format!("{name} across {} file(s)", files.len())
                }
                TargetId::Unrecorded(n) => format!("unrecorded {n}"),
                TargetId::Field {
                    file,
                    strukt,
                    field,
                } => format!("field {strukt}.{field} in {}", short_path(file)),
            })
            .collect::<Vec<_>>()
            .join(" + ")
    };
    format!(
        "{} bytes {}..{} ({}): {} -> {}",
        short_path(&key.file),
        key.start,
        key.end,
        key.kind.as_str(),
        show(before),
        show(after)
    )
}

pub(crate) fn short_path(id: &str) -> String {
    let p = Path::new(id);
    std::env::current_dir()
        .ok()
        .and_then(|cwd| cwd.canonicalize().ok())
        .and_then(|cwd| p.strip_prefix(&cwd).ok().map(|r| r.display().to_string()))
        .unwrap_or_else(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_map_through_insertions_and_replacements() {
        let edits = vec![
            Edit::new((0, 0), "pub "),
            Edit::new((10, 10), "std::"),
            Edit::new((20, 24), " : "),
            Edit {
                range: (27, 27),
                text: ")".into(),
                closes: true,
            },
        ];
        assert_eq!(map_offset(&edits, 0), 0);
        assert_eq!(map_offset(&edits, 5), 9);
        assert_eq!(map_offset(&edits, 10), 14);
        assert_eq!(map_offset(&edits, 11), 20);
        assert_eq!(map_offset(&edits, 24), 32);
        assert_eq!(map_offset(&edits, 27), 36);
        assert_eq!(
            apply_edits("0123456789abcdefghij::  xyz", &edits).unwrap(),
            "pub 0123456789std::abcdefghij : xyz)"
        );
    }
}
