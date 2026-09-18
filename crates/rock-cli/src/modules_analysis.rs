//! The analysis `rock modules audit` and `rock modules migrate` share (spec
//! 15): one population rule, one checked workspace, and one walk listing every
//! reference a lowered body or signature resolved, site by site. The audit
//! aggregates the list into counts; the migrate planner reads individual
//! sites (what a consumer uses, what a fragment needs imported, which call a
//! candidate rebinds).

use std::collections::{BTreeMap, HashMap};
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

// --- edits and site comparison (migrate and fix --imports) -------------------------------------------------------------------

/// One span edit: replace `range` (empty for an insertion) with `text`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Edit {
    pub range: (usize, usize),
    pub text: String,
}

/// Apply non-overlapping edits. Overlap is refused (spec 15.2).
pub(crate) fn apply_edits(text: &str, edits: &[Edit]) -> std::result::Result<String, String> {
    let mut sorted: Vec<&Edit> = edits.iter().collect();
    sorted.sort_by_key(|e| e.range);
    let mut out = String::with_capacity(text.len() + 64);
    let mut at = 0;
    for e in sorted {
        if e.range.0 < at {
            return Err(format!("overlapping edits at byte {}", e.range.0));
        }
        if e.range.1 > text.len() || !text.is_char_boundary(e.range.0) || !text.is_char_boundary(e.range.1) {
            return Err(format!("edit range {:?} is outside the file", e.range));
        }
        out.push_str(&text[at..e.range.0]);
        out.push_str(&e.text);
        at = e.range.1;
    }
    out.push_str(&text[at..]);
    Ok(out)
}

/// Where a baseline byte offset lands after `edits`. An insertion AT an offset
/// stays before it (a qualified call starts where its name started); a
/// replaced range collapses onto its start.
pub(crate) fn map_offset(edits: &[Edit], off: usize) -> usize {
    let mut shift: isize = 0;
    for e in edits {
        let (a, b) = e.range;
        if a == b {
            if off > a {
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
    Fn { file: Ustr, name: String, ordinal: usize },
    Def { file: Ustr, name: String },
    Family { files: Vec<Ustr>, name: String },
    Unrecorded(String),
}

pub(crate) fn target_id(ws: &Workspace, t: &Target) -> TargetId {
    match t {
        Target::Fn(mid, fid) => {
            let m = ws.module(*mid);
            let name = declared_name(m.functions[*fid].name.as_str()).to_string();
            let ordinal = m
                .functions
                .iter()
                .filter(|(i, f)| u32::from(i.into_raw()) < u32::from(fid.into_raw()) && declared_name(f.name.as_str()) == name)
                .count();
            TargetId::Fn { file: m.file, name, ordinal }
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
/// candidate coordinates (empty for a candidate). Types and patterns are
/// compared per file as a multiset rather than by site: a v1 signature type
/// records no span (`span_recordable` tests the spelled name against maps
/// keyed by DefKey), and a type name cannot be overloaded, so a per-file
/// count is as strict.
pub(crate) fn sites(ws: &Workspace, refs: &[Ref], file_pkg: &HashMap<Ustr, u32>, edits: &HashMap<Ustr, Vec<Edit>>) -> Sites {
    let mut out: Sites = BTreeMap::new();
    for r in refs {
        let file = ws.module(r.from).file;
        if !file_pkg.contains_key(&file) {
            continue;
        }
        if matches!(r.kind, RefKind::Type | RefKind::Pattern) {
            out.entry(SiteKey { file, start: 0, end: 0, kind: r.kind })
                .or_default()
                .push(target_id(ws, &r.target));
            continue;
        }
        let Some(span) = r.span else { continue };
        if span.file != file {
            continue;
        }
        let (start, end) = match edits.get(&file) {
            Some(e) => (map_offset(e, span.offset_range.0), map_offset(e, span.offset_range.1)),
            None => span.offset_range,
        };
        out.entry(SiteKey { file, start, end, kind: r.kind })
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
                Some(_) if matches!(k.kind, RefKind::Type | RefKind::Pattern) => k.clone(),
                Some(e) => SiteKey { file: k.file, start: map_offset(e, k.start), end: map_offset(e, k.end), kind: k.kind },
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
                TargetId::Fn { file, name, ordinal } => format!("fn {name}#{ordinal} in {}", short_path(file)),
                TargetId::Def { file, name } => format!("{name} in {}", short_path(file)),
                TargetId::Family { files, name } => format!("{name} across {} file(s)", files.len()),
                TargetId::Unrecorded(n) => format!("unrecorded {n}"),
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
            Edit { range: (0, 0), text: "pub ".into() },
            Edit { range: (10, 10), text: "std::".into() },
            Edit { range: (20, 24), text: " : ".into() },
        ];
        assert_eq!(map_offset(&edits, 0), 0);
        assert_eq!(map_offset(&edits, 5), 9);
        assert_eq!(map_offset(&edits, 10), 14);
        assert_eq!(map_offset(&edits, 11), 20);
        assert_eq!(map_offset(&edits, 24), 32);
        assert_eq!(apply_edits("0123456789abcdefghij::  xyz", &edits).unwrap(), "pub 0123456789std::abcdefghij : xyz");
    }
}
