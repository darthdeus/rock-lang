//! The analysis `rock modules audit` and `rock modules migrate` share (spec
//! 15): one population rule, one checked workspace, and one walk listing every
//! reference a lowered body or signature resolved, site by site. The audit
//! aggregates the list into counts; the migrate planner reads individual
//! sites (what a consumer uses, what a fragment needs imported, which call a
//! candidate rebinds).

use std::collections::HashMap;
use std::path::Path;

use bone_core::hir::{Callee, Expr, Function, Pattern, TypeCtor, TypeExpr};
use bone_core::prelude::{Idx, ModuleId};
use bone_core::workspace::{BoundNext, BoundTree};
use bone_semantic::inference::Workspace;
use rock_core::errors::CompilerError;
use rock_core::source_code::Span;
use rock_core::SourceFiles;
use rock_host::stick::graph::PackageGraph;
use ustr::Ustr;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PopMode {
    /// The `rock compile` set: files under a `tests/` path are dropped.
    Production,
    /// The whole-package set `rock test <dir>` builds.
    Test,
}

pub(crate) struct Population {
    pub mode: PopMode,
    pub discovered: SourceFiles,
    pub analyzed: SourceFiles,
}

/// The sources `project` compiles as. A root that itself lives under a
/// `tests/` path would be analyzed empty by the `rock compile` rule, so it
/// takes the `rock test` set unless the caller names a mode.
pub(crate) fn population(
    project: &Path,
    graph: &PackageGraph,
    mode: Option<PopMode>,
) -> Result<Population, String> {
    let spec = crate::unified_compile::SourceSpec::Directory(project.to_path_buf());
    let discovered = crate::unified_compile::discover_sources(&spec).map_err(|e| format!("{e:#}"))?;
    let root_path = graph.package(graph.root()).root.to_string_lossy().into_owned();
    let mode = mode.unwrap_or(if SourceFiles::is_test_only_path(&root_path) {
        PopMode::Test
    } else {
        PopMode::Production
    });
    let analyzed = match mode {
        PopMode::Production => discovered.production_only(),
        PopMode::Test => discovered.clone(),
    };
    Ok(Population { mode, discovered, analyzed })
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
/// spec 11 closure to the caller (`api_closure_requirements`).
pub(crate) fn check(sources: &SourceFiles) -> Result<Checked, CheckFailure> {
    let (mut modules, _entry) = crate::skull_backend::skull_assemble_project(sources)
        .map_err(|errors| CheckFailure { stage: "parse", errors })?;
    let census = modules.iter().map(|m| (m.file, census(m))).collect();
    let (ws, errors) = crate::bone_backend::check_modules_for_tools(&mut modules)
        .map_err(|errors| CheckFailure { stage: "semantic pass", errors })?;
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
        if item.annotations.iter().any(|a| a.name.name.as_str() == "nomangle") {
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
        }
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
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } => Some(*name),
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
                    target: Target::Key { key: name, modules: type_modules(name) },
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
                        target: Target::Key { key: name, modules: type_modules(name) },
                        callee: None,
                    });
                }
            }
            for (_, pat) in body.pattern_arena.iter() {
                let (name, span) = match pat {
                    Pattern::EnumVariant { enum_name: Some(name), enum_name_span, .. } => (*name, *enum_name_span),
                    Pattern::Record { enum_name: Some(name), enum_name_span, .. } => (*name, *enum_name_span),
                    Pattern::Record { enum_name: None, name: Some(name), name_span, .. } => (*name, *name_span),
                    _ => continue,
                };
                out.push(Ref {
                    from: m.id,
                    span,
                    kind: RefKind::Pattern,
                    target: Target::Key { key: name, modules: type_modules(name) },
                    callee: None,
                });
            }
            for (idx, expr) in body.expr_arena.iter() {
                let span = body.expr_spans.get(idx).copied();
                let site = match expr {
                    Expr::Call { callee: Callee::Unresolved { name, name_span }, args, .. } => Some(CalleeSite {
                        name: *name,
                        name_span: *name_span,
                        method: !args.is_empty() && body.method_call_recvs.contains(&args.start()),
                    }),
                    _ => None,
                };
                // Every call inference bound, whatever its syntax (named, UFCS,
                // operator, compound assignment, a callee that is a value).
                let bound = bi.and_then(|bi| bi.resolved_overload_calls.get(&idx)).copied();
                if let Some((mid, fid)) = bound {
                    out.push(Ref { from: m.id, span, kind: RefKind::Call, target: Target::Fn(mid, fid), callee: site });
                }
                let (key, modules, kind) = match expr {
                    Expr::Call { callee: Callee::Unresolved { name, .. }, .. } => {
                        if bound.is_some() {
                            continue;
                        }
                        // Inference bound no function: a newtype constructor or
                        // a call through a fn-valued global, attributed like the
                        // definition it names.
                        let types = type_modules(*name);
                        let kind = if types.is_empty() { RefKind::Value } else { RefKind::Construction };
                        let mut mids = global_modules.get(name).cloned().unwrap_or_default();
                        mids.extend(types);
                        if mids.is_empty() {
                            out.push(Ref { from: m.id, span, kind: RefKind::Call, target: Target::Unrecorded(*name), callee: site });
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
                    Expr::StructLiteral { name, .. } => (*name, type_modules(*name), RefKind::Construction),
                    Expr::EnumVariant { enum_name, .. } => (*enum_name, type_modules(*enum_name), RefKind::EnumVariant),
                    _ => continue,
                };
                out.push(Ref { from: m.id, span, kind, target: Target::Key { key, modules }, callee: site });
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

/// What one inline Lua body references, as LuaJIT compiles it (spec 13: raw
/// references to generated Rock names, `_G` accesses and reflection strings).
/// Exact: a local, a parameter, a field, a table key, a comment or a string is
/// not a free global, and the Lua compiler is the one deciding.
#[derive(Debug, Default)]
pub(crate) struct LuaRefs {
    /// Free globals the body (or a function nested in it) reads or writes.
    pub globals: Vec<String>,
    /// String constants and constant field keys, when the body reaches `_G`:
    /// a name it can look up there (`_G.x`, `_G["x"]`, `rawget(_G, "x")`).
    pub via_g: Vec<String>,
    /// The body does not compile as Lua; its references are unknown.
    pub error: Option<String>,
}

/// Compile every body with the toolchain's LuaJIT and read its bytecode
/// (`jit.bc`): GGET/GSET name the free globals, and a body that reads `_G`
/// also reports its string constants. One LuaJIT process for all bodies. An
/// `Err` means the census could not run at all.
pub(crate) fn lua_references(bodies: &[LuaBody]) -> Result<Vec<LuaRefs>, String> {
    if bodies.is_empty() {
        return Ok(Vec::new());
    }
    let mut script = String::from("local bc = require(\"jit.bc\")\nlocal bodies = {\n");
    for b in bodies {
        let src = if b.returns_value && !b.src.as_str().contains("return") {
            format!("return {}", b.src.as_str().trim())
        } else {
            b.src.to_string()
        };
        let params = b.params.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(", ");
        script.push_str(&format!("  {},\n", lua_quote(&format!("return function({params})\n{src}\nend"))));
    }
    script.push_str(
        r#"}
for i, text in ipairs(bodies) do
  local chunk, err = loadstring(text, "=inline")
  if not chunk then
    io.write("E\t", i, "\t", (tostring(err):gsub("[\t\n]", " ")), "\n")
  else
    local out = {}
    bc.dump(chunk(), { write = function(_, s) out[#out + 1] = s end, flush = function() end }, true)
    for line in table.concat(out):gmatch("[^\n]+") do
      io.write("L\t", i, "\t", line, "\n")
    end
  end
end
"#,
    );
    let run = rock_host::lua_runner::run_lua_code_with_timeout(&script, Some(std::time::Duration::from_secs(60)))
        .map_err(|e| format!("the raw-ABI census could not run LuaJIT: {}", e.message))?;
    if !run.success {
        return Err(format!("the raw-ABI census failed in LuaJIT: {}", run.stderr.trim()));
    }
    let mut refs: Vec<LuaRefs> = bodies.iter().map(|_| LuaRefs::default()).collect();
    let mut strings: Vec<Vec<String>> = bodies.iter().map(|_| Vec::new()).collect();
    let mut reaches_g = vec![false; bodies.len()];
    for line in run.stdout.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(tag), Some(index), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(format!("unexpected raw-ABI census output: {line}"));
        };
        let i = index
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .filter(|&n| n < bodies.len())
            .ok_or_else(|| format!("unexpected raw-ABI census output: {line}"))?;
        if tag == "E" {
            refs[i].error = Some(rest.to_string());
            continue;
        }
        // `0003    GGET     4   1      ; "x"`, a jump target as `0014 => OP`.
        let mut words = rest.split_whitespace().skip(1).skip_while(|w| *w == "=>");
        let Some(op) = words.next() else { continue };
        let Some(constant) = rest.split_once("; \"").and_then(|(_, k)| k.strip_suffix('"')) else { continue };
        match op {
            "GGET" | "GSET" => {
                if constant == "_G" {
                    reaches_g[i] = true;
                }
                refs[i].globals.push(constant.to_string());
            }
            "KSTR" | "TGETS" | "TSETS" => strings[i].push(constant.to_string()),
            _ => {}
        }
    }
    for (i, r) in refs.iter_mut().enumerate() {
        r.globals.sort();
        r.globals.dedup();
        if reaches_g[i] {
            r.via_g = std::mem::take(&mut strings[i]);
            r.via_g.sort();
            r.via_g.dedup();
        }
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
