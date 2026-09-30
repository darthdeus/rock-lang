//! `rock fix --across-yield <file>... [--apply]`: the sim world-entry rewrite
//! of docs/identity-storage-plan.md ("Sim rewrite shards", steps 1-6) for
//! MAG-shaped sims, planned on the checked program.
//!
//! Stage one gives each `@sim co fn` the `world: Scene<WorldState>` entry
//! parameter and replaces the `MAG_ACTIVE_WORLD` chains; a local named
//! `world` becomes `state`. Stage two reads the identity borrows that cross a
//! suspension (`bone_core::suspension::suspension_crossings`) and, for a
//! borrow whose `let` initializer resolves a handle rooted in a parameter or
//! an unreassigned local, rebinds it after each suspension it crosses, and
//! inlines the resolution into escaping closures that capture it. It never
//! re-resolves from a global: that could swap "this object" for "the current
//! one" (A2). Any other borrow is left as written and reported.
//!
//! Each stage is re-checked in memory; a file whose stage adds an error the
//! baseline lacks gets that stage reverted and is reported. Files that switch
//! worlds, drive the run lifecycle, restore a WorldImage or boot the menu are
//! skipped whole (the flags of `tools/sim_world_entry_shards.py`).

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use atomic_refcell::AtomicRefCell;
use bone_core::hir::{BindingKind, Body, BodyOwner, Expr, Function, Module, Statement};
use bone_core::prelude::{FxHashMap, Idx, Span};
use bone_semantic::hir::BindingInfo;
use bone_semantic::identity_storage::{identity_module_body_sites, identity_suspension_crossings, IdentityStorageKind};
use bone_semantic::inference::Workspace;
use regex_lite::Regex;
use rock_core::errors::CompilerError;
use rock_core::SourceFiles;
use ustr::Ustr;

use crate::modules_analysis::{apply_edits, Edit};

const EXIT_NOT_CLEAN: i32 = 3;
/// A reverted file leaves the next round with fewer edits, so the rounds are
/// bounded by the file count; the cap only guards the loop.
const MAX_ROUNDS: usize = 16;

pub fn run(files: &[String], apply: bool) -> Result<i32> {
    let mut plan = Plan::load(files)?;
    let base = check(&plan.sources, &plan.texts(|f| &f.original))
        .map_err(|errs| anyhow::anyhow!("rock fix --across-yield: the baseline does not check:\n{}", first_errors(&errs)))?;
    for f in plan.files.iter_mut().filter(|f| f.skipped.is_none()) {
        let Some(module) = module_of(&base.ws, f.id) else {
            anyhow::bail!("rock fix --across-yield: `{}` is not a module of {}", f.rel, plan.root.display());
        };
        let (edits, refused) = entry_edits(module, f.id, &f.original);
        f.refused = refused;
        f.entry = apply_edits(&f.original, &edits).map_err(|e| anyhow::anyhow!(e))?;
    }
    let entry = plan.settle(&base, "entry", |f| &f.entry, |f| {
        let changed = f.entry != f.original;
        f.entry = f.original.clone();
        changed
    })?;
    for f in plan.files.iter_mut().filter(|f| f.skipped.is_none()) {
        let module = module_of(&entry.ws, f.id).expect("checked above");
        let (edits, refused) = helper_edits(&entry.ws, module, f.id, &f.entry);
        f.refused.extend(refused);
        f.helpers = apply_edits(&f.entry, &edits).map_err(|e| anyhow::anyhow!(e))?;
    }
    // New local borrows in helpers need their rebindings before the final
    // error comparison. The checked candidate still exposes those crossings.
    let helpers = match check(&plan.sources, &plan.texts(|f| &f.helpers)) {
        Ok(checked) => checked,
        Err(_) => plan.settle(&base, "helpers", |f| &f.helpers, |f| {
            let changed = f.helpers != f.entry;
            f.helpers = f.entry.clone();
            changed
        })?,
    };
    for f in plan.files.iter_mut().filter(|f| f.skipped.is_none()) {
        let module = module_of(&helpers.ws, f.id).expect("checked above");
        let (edits, left) = rebind_edits(&helpers.ws, module, f.id, &f.helpers);
        f.left = left;
        f.rebound = apply_edits(&f.helpers, &edits).map_err(|e| anyhow::anyhow!(e))?;
    }
    let done = plan.settle(&base, "rebind", |f| &f.rebound, |f| {
        let changed = f.rebound != f.entry;
        f.rebound = f.entry.clone();
        f.left.clear();
        changed
    })?;

    let mut out = format!("rock fix --across-yield: {} files under {}\n", plan.files.len(), plan.root.display());
    let (mut before, mut after, mut changed) = ([0; 3], [0; 3], 0);
    let mut clean = true;
    for f in &plan.files {
        if let Some(flag) = &f.skipped {
            let _ = writeln!(out, "{}: skipped ({flag})", f.rel);
            clean = false;
            continue;
        }
        let b = site_counts(&base.ws, f.id);
        let a = site_counts(&done.ws, f.id);
        for k in 0..3 {
            before[k] += b[k];
            after[k] += a[k];
        }
        changed += usize::from(f.rebound != f.original);
        let _ = writeln!(out, "{}: across-yield {} -> {}, capture {} -> {}, co-param {} -> {}", f.rel, b[0], a[0], b[1], a[1], b[2], a[2]);
        for line in f.refused.iter().chain(&f.left) {
            let _ = writeln!(out, "  {line}");
        }
        clean &= f.refused.is_empty() && f.left.is_empty() && a == [0, 0, 0];
    }
    let _ = writeln!(
        out,
        "total: across-yield {} -> {}, capture {} -> {}, co-param {} -> {}; {changed} files {}",
        before[0],
        after[0],
        before[1],
        after[1],
        before[2],
        after[2],
        if apply { "written" } else { "would change (--apply writes)" }
    );
    if apply {
        for f in plan.files.iter().filter(|f| f.skipped.is_none() && f.rebound != f.original) {
            fs_err::write(&f.path, &f.rebound)?;
        }
    }
    print!("{out}");
    Ok(if clean && (apply || changed == 0) { 0 } else { EXIT_NOT_CLEAN })
}

struct FixFile {
    id: Ustr,
    path: PathBuf,
    rel: String,
    original: String,
    /// After stage one: entry parameter and world chains.
    entry: String,
    /// After the checked coroutine-helper parameter and caller rewrite.
    helpers: String,
    /// After stage two: rebindings and closure resolutions.
    rebound: String,
    skipped: Option<&'static str>,
    refused: Vec<String>,
    left: Vec<String>,
}

struct Plan {
    root: PathBuf,
    sources: SourceFiles,
    files: Vec<FixFile>,
}

struct Checked {
    ws: Workspace,
    errors: Vec<CompilerError>,
}

impl Plan {
    fn load(files: &[String]) -> Result<Plan> {
        let mut paths = Vec::new();
        for f in files {
            let p = Path::new(f).canonicalize().map_err(|e| anyhow::anyhow!("rock fix --across-yield: `{f}`: {e}"))?;
            paths.push(p);
        }
        let root_of = |p: &Path| p.ancestors().skip(1).find(|d| d.join("stick.toml").is_file()).map(Path::to_path_buf);
        let root = root_of(&paths[0])
            .ok_or_else(|| anyhow::anyhow!("rock fix --across-yield: no stick.toml above `{}`", paths[0].display()))?;
        if let Some(p) = paths.iter().find(|p| root_of(p).as_ref() != Some(&root)) {
            anyhow::bail!("rock fix --across-yield: `{}` is not in the package at {}", p.display(), root.display());
        }
        let sources = crate::unified_compile::discover_sources(&crate::unified_compile::SourceSpec::Directory(root.clone()))?;
        let mut out = Vec::new();
        for path in paths {
            let found = sources.files.iter().find(|f| Path::new(&f.borrow().file.to_id_path()) == path);
            let Some(found) = found else {
                anyhow::bail!("rock fix --across-yield: `{}` is not a source of {}", path.display(), root.display());
            };
            let f = found.borrow();
            let original = f.contents.clone();
            out.push(FixFile {
                id: Ustr::from(f.file.to_id_path().as_str()),
                rel: path.strip_prefix(root.parent().unwrap_or(&root)).unwrap_or(&path).display().to_string(),
                path,
                skipped: world_semantics(&original),
                entry: original.clone(),
                helpers: original.clone(),
                rebound: original.clone(),
                original,
                refused: Vec::new(),
                left: Vec::new(),
            });
        }
        Ok(Plan { root, sources, files: out })
    }

    fn texts(&self, stage: impl Fn(&FixFile) -> &String) -> HashMap<Ustr, String> {
        self.files.iter().map(|f| (f.id, stage(f).clone())).collect()
    }

    /// Checks the stage's texts, reverting the stage in each file that gained
    /// an error the baseline lacks until none does.
    fn settle(
        &mut self,
        base: &Checked,
        stage: &str,
        text: impl Fn(&FixFile) -> &String,
        revert: impl Fn(&mut FixFile) -> bool,
    ) -> Result<Checked> {
        for _ in 0..MAX_ROUNDS {
            let checked = check(&self.sources, &self.texts(&text));
            let errors = match &checked {
                Ok(c) => new_errors(&base.errors, &c.errors),
                Err(errs) => new_errors(&base.errors, &flatten(errs.clone())),
            };
            if errors.is_empty() {
                if let Ok(c) = checked {
                    return Ok(c);
                }
            }
            let mut reverted = false;
            for f in self.files.iter_mut() {
                let Some(e) = errors.iter().find(|e| e.location.file == f.id) else { continue };
                if f.skipped.is_some() || !revert(f) {
                    continue;
                }
                f.refused.push(format!("refused {stage}: line {}: {}", e.location.line + 1, e.message));
                reverted = true;
            }
            if !reverted {
                anyhow::bail!("rock fix --across-yield: the {stage} stage adds errors outside the rewritten files:\n{}", first_errors(&errors));
            }
        }
        anyhow::bail!("rock fix --across-yield: the {stage} stage did not settle in {MAX_ROUNDS} rounds")
    }
}

fn check(sources: &SourceFiles, texts: &HashMap<Ustr, String>) -> std::result::Result<Checked, Vec<CompilerError>> {
    let sources = SourceFiles {
        files: sources
            .files
            .iter()
            .map(|f| {
                let mut f = f.borrow().clone();
                if let Some(text) = texts.get(&Ustr::from(f.file.to_id_path().as_str())) {
                    f.contents = text.clone();
                }
                Arc::new(AtomicRefCell::new(f))
            })
            .collect(),
        package_roots: sources.package_roots.clone(),
    };
    let (mut modules, _entry) = crate::skull_backend::skull_assemble_project(&sources)?;
    let (ws, errors) = crate::bone_backend::check_modules_for_tools(&mut modules)?;
    Ok(Checked { ws, errors: flatten(errors) })
}

fn flatten(errors: Vec<CompilerError>) -> Vec<CompilerError> {
    let mut out = Vec::new();
    for mut e in errors {
        let siblings = std::mem::take(&mut e.siblings);
        out.push(e);
        out.extend(siblings);
    }
    out
}

/// Errors in `now` beyond the baseline's count of the same file and message.
/// Lines move with the edits, so they are not part of the key.
fn new_errors(base: &[CompilerError], now: &[CompilerError]) -> Vec<CompilerError> {
    let mut left: HashMap<(Ustr, &str), usize> = HashMap::new();
    for e in base {
        *left.entry((e.location.file, e.message.as_str())).or_default() += 1;
    }
    let mut out = Vec::new();
    for e in now {
        match left.get_mut(&(e.location.file, e.message.as_str())) {
            Some(n) if *n > 0 => *n -= 1,
            _ => out.push(e.clone()),
        }
    }
    out
}

fn first_errors(errors: &[CompilerError]) -> String {
    errors.iter().take(5).map(|e| format!("  {}:{}: {}\n", e.location.file, e.location.line + 1, e.message)).collect()
}

fn module_of(ws: &Workspace, id: Ustr) -> Option<&Module> {
    ws.modules.iter().find(|m| m.file == id)
}

/// The world-semantics flags of `tools/sim_world_entry_shards.py`.
fn world_semantics(src: &str) -> Option<&'static str> {
    let run = Regex::new(
        r"\b(run_start|run_start_deferred|run_prepare\w*|run_stop|run_close|run_request_play|run_retarget_unrequested|run_discard_unrequested|world_load|mag_run_update)\(",
    )
    .expect("static regex");
    if src.contains("MAG_BOOT_MENU") || src.contains("MAG_MENU_AUTOPREP") {
        Some("menu-boot")
    } else if src.contains("mag_set_active_world") {
        Some("world-switch")
    } else if run.is_match(src) {
        Some("run-lifecycle")
    } else if src.contains("snapshot_restore") || src.to_lowercase().contains("world_image") {
        Some("restore")
    } else {
        None
    }
}

fn functions_in<'m>(module: &'m Module, id: Ustr) -> impl Iterator<Item = (Idx<Function>, &'m Function, &'m Body)> {
    module.functions.iter().filter_map(move |(fid, f)| {
        let body = module.bodies.get(&BodyOwner::Function(fid))?;
        (f.span.file == id && !f.is_synthetic).then_some((fid, f, body))
    })
}

fn is_sim(f: &Function) -> bool {
    f.is_coroutine && f.annotations.iter().any(|a| a.name.name.as_str() == "sim")
}

fn slice(text: &str, span: Span) -> &str {
    &text[span.offset_range.0..span.offset_range.1]
}

fn contains(outer: Span, inner: Span) -> bool {
    outer.offset_range.0 <= inner.offset_range.0 && inner.offset_range.1 <= outer.offset_range.1
}

/// Stage one (recipe steps 1-3 and 6).
fn entry_edits(module: &Module, id: Ustr, text: &str) -> (Vec<Edit>, Vec<String>) {
    let mut edits = Vec::new();
    let mut refused = Vec::new();
    for (_, f, body) in functions_in(module, id) {
        match function_entry_edits(f, body, text) {
            Ok(e) => edits.extend(e),
            Err(why) => refused.push(format!("refused `{}`: {why}", f.name)),
        }
    }
    (edits, refused)
}

fn function_entry_edits(f: &Function, body: &Body, text: &str) -> std::result::Result<Vec<Edit>, String> {
    let chain = Regex::new(r"\bMAG_ACTIVE_WORLD\b(\.unwrap\(\)(\.get\(\)\.unwrap\(\)(\.player\.get\(\)\.unwrap\(\))?)?)?")
        .expect("static regex");
    let resolvers = Regex::new(r"\b(mag_world_state|active_world_scene)\(\)(\.player\.get\(\)\.unwrap\(\))?").expect("static regex");
    let param_player = Regex::new(r"\bworld\.get\(\)\.unwrap\(\)\.player\.get\(\)\.unwrap\(\)").expect("static regex");
    let mut edits = Vec::new();
    let (start, end) = f.span.offset_range;
    let entry = is_sim(f) && (f.param_decls.is_empty() || f.param_decls.iter().map(|p| p.name.as_str()).eq(["world"]));
    if entry && f.param_decls.is_empty() {
        let open = f.name_span.offset_range.1 + text[f.name_span.offset_range.1..end].find('(').ok_or("no parameter list")?;
        if !text[open + 1..].trim_start().starts_with(')') {
            return Err("unexpected parameter list".to_string());
        }
        edits.push(Edit::new((open + 1, open + 1), "world: Scene<WorldState>"));
    }
    let renamed = if entry { rename_world_local(body, text)? } else { Vec::new() };
    let local_world = !renamed.is_empty();
    edits.extend(renamed);
    for m in chain.captures_iter(&text[start..end]) {
        let whole = m.get(0).expect("match");
        let replacement = match (m.get(1), m.get(2), m.get(3), entry) {
            (None, ..) => return Err("reads MAG_ACTIVE_WORLD other than through `.unwrap()`".to_string()),
            (_, _, Some(_), true) => "world.player()",
            (_, Some(_), None, true) => "world.get().unwrap()",
            (_, None, None, true) => "world",
            (_, _, Some(_), false) => "mag_world_state().player.get().unwrap()",
            (_, Some(_), None, false) => "mag_world_state()",
            (_, None, None, false) => "active_world_scene()",
        };
        edits.push(Edit::new((start + whole.start(), start + whole.end()), replacement));
    }
    if entry {
        for m in resolvers.captures_iter(&text[start..end]) {
            let whole = m.get(0).expect("match");
            let replacement = match (&m[1], m.get(2)) {
                ("mag_world_state", Some(_)) => "world.player()",
                ("mag_world_state", None) => "world.get().unwrap()",
                _ => "world",
            };
            edits.push(Edit::new((start + whole.start(), start + whole.end()), replacement));
        }
        if !local_world {
            for m in param_player.find_iter(&text[start..end]) {
                edits.push(Edit::new((start + m.start(), start + m.end()), "world.player()"));
            }
        }
    }
    Ok(edits)
}

/// A sim's `let world = <the active WorldState>` becomes `state`, so the
/// name is free for the entry parameter; `state.player.get().unwrap()`
/// becomes `world.player()`.
fn rename_world_local(body: &Body, text: &str) -> std::result::Result<Vec<Edit>, String> {
    let named = |name: &'static str| {
        body.binding_arena.iter().filter(move |(_, b)| b.name.as_str() == name && b.kind != BindingKind::Param)
    };
    let Some((world, _)) = named("world").next() else { return Ok(Vec::new()) };
    let init = let_value(body, world).map(|v| slice(text, body.expr_spans[v]).trim());
    let is_state = init.is_some_and(|i| {
        i == "mag_world_state()" || i == "MAG_ACTIVE_WORLD.unwrap().get().unwrap()" || i == "world.get().unwrap()"
    });
    if !is_state || named("world").count() > 1 {
        return Err("a local `world` that is not the active WorldState would shadow the entry parameter".to_string());
    }
    let state = ["state", "world_state"]
        .into_iter()
        .find(|name| named(name).next().is_none())
        .ok_or("`state` and `world_state` are both taken")?;
    let mut edits = vec![Edit::new(body.binding_spans[world].offset_range, state)];
    const PLAYER: &str = ".player.get().unwrap()";
    for (e, expr) in body.expr_arena.iter() {
        if !matches!(expr, Expr::Var(b) if *b == world) {
            continue;
        }
        let (s, t) = body.expr_spans[e].offset_range;
        if text[t..].starts_with(PLAYER) {
            edits.push(Edit::new((s, t + PLAYER.len()), "world.player()"));
        } else {
            edits.push(Edit::new((s, t), state));
        }
    }
    Ok(edits)
}

struct HelperParam {
    index: usize,
    binding: Idx<BindingInfo>,
    identity: String,
}

/// All callers must migrate with the declaration. Refuse this file's helper
/// group if one caller cannot carry the same scene, including helper chains.
fn helper_edits(ws: &Workspace, module: &Module, id: Ustr, text: &str) -> (Vec<Edit>, Vec<String>) {
    let mut params = BTreeMap::new();
    let mut refused = Vec::new();
    for (fid, f, body) in functions_in(module, id).filter(|(_, f, _)| f.is_coroutine && !is_sim(f)) {
        let identities: Vec<_> = f.param_decls.iter().enumerate().filter_map(|(index, p)| {
            let ty = ws.render(None, p.ty);
            let identity = ty.rsplit("::").next().unwrap_or(&ty);
            matches!(identity, "Player" | "WorldState" | "Mob").then(|| (index, identity.to_string()))
        }).collect();
        if identities.is_empty() {
            continue;
        }
        let root = body.root.expect("function body");
        let header_start = text[..f.name_span.offset_range.0].rfind('\n').map_or(0, |i| i + 1);
        let header = &text[header_start..body.expr_spans[root].offset_range.0];
        let why = if Regex::new(r"co fn \w+\.").expect("static regex").is_match(header) {
            Some("receiver-style co helper")
        } else if Regex::new(r"Coroutine<[^,>]+,\s*(Player|WorldState|Mob)\b").expect("static regex").is_match(header) {
            Some("co helper returns an identity")
        } else if identities.len() != 1 {
            Some("co helper has multiple identity parameters")
        } else if identities[0].1 == "Mob" {
            Some("Mob co helper needs Entity forwarding")
        } else if body.binding_arena.iter().any(|(_, b)| b.name.as_str() == "world") {
            Some("co helper already binds `world`")
        } else if f.param_decls[identities[0].0].has_default {
            Some("identity parameter has a default")
        } else {
            None
        };
        if let Some(why) = why {
            refused.push(format!("refused helper `{}`: {why}", f.name));
            continue;
        }
        let (index, identity) = identities.into_iter().next().expect("one identity");
        let binding = body.binding_arena.iter().find(|(_, b)| b.kind == BindingKind::Param && b.name == f.param_decls[index].name)
            .map(|(b, _)| b).expect("parameter binding");
        params.insert(fid, HelperParam { index, binding, identity });
    }
    if params.is_empty() {
        return (Vec::new(), refused);
    }
    let mut edits = Vec::new();
    for (&fid, param) in &params {
        let f = &module.functions[fid];
        let body = &module.bodies[&BodyOwner::Function(fid)];
        match helper_parameter_edits(ws, module, id, text, fid, param, &params) {
            Ok(e) => edits.extend(e),
            Err(why) => refused.push(format!("refused helper `{}`: {why}", f.name)),
        }
        let root = body.root.expect("function body");
        let start = body.expr_spans[root].offset_range.0;
        let header = &text[f.span.offset_range.0..start];
        let name = f.param_decls[param.index].name;
        let declaration = Regex::new(&format!(r"\b{name}\s*:\s*(?:\w+::)*{}\b", param.identity)).expect("identifier regex");
        let Some(found) = declaration.find(header) else {
            refused.push(format!("refused helper `{}`: unsupported parameter declaration", f.name));
            continue;
        };
        edits.push(Edit::new((f.span.offset_range.0 + found.start(), f.span.offset_range.0 + found.end()), "world: Scene<WorldState>"));
        let line_start = text[..f.name_span.offset_range.0].rfind('\n').map_or(0, |i| i + 1);
        let indent: String = text[line_start..f.name_span.offset_range.0].chars().take_while(|c| c.is_whitespace()).collect();
        let resolution = if param.identity == "Player" { "world.player()" } else { "world.get().unwrap()" };
        edits.push(Edit::new((start + 1, start + 1), format!("\n{indent}    let {name} = {resolution}")));
    }
    if refused.is_empty() { (edits, refused) } else { (Vec::new(), refused) }
}

fn helper_parameter_edits(
    ws: &Workspace, module: &Module, id: Ustr, text: &str, fid: Idx<Function>, param: &HelperParam,
    params: &BTreeMap<Idx<Function>, HelperParam>,
) -> std::result::Result<Vec<Edit>, String> {
    let f = &module.functions[fid];
    let mut edits = Vec::new();
    for m in &ws.modules {
        let inference = ws.inference(m.id);
        for (owner, body) in &m.bodies {
            let bi = inference.and_then(|i| i.per_fn.get(owner));
            for (e, expr) in body.expr_arena.iter() {
                if matches!(expr, Expr::SiblingRef(name) | Expr::Global(name) if *name == f.name) {
                    return Err("helper is used as a function value".to_string());
                }
                if bi.and_then(|bi| bi.resolved_overload_calls.get(&e)) != Some(&(module.id, fid)) {
                    continue;
                }
                if body.expr_spans[e].file != id {
                    return Err("called from another file".to_string());
                }
                let BodyOwner::Function(caller) = owner else { return Err("caller is not a function".to_string()) };
                let Expr::Call { args, .. } = expr else { return Err("unsupported call shape".to_string()) };
                if args.clone().any(|a| body.spread_args.contains(&a)) {
                    return Err("caller uses spread arguments".to_string());
                }
                let mut positional = 0;
                let arg = args.clone().find(|a| match body.arg_names.get(a) {
                    Some(name) => *name == f.param_decls[param.index].name,
                    None => { let index = positional; positional += 1; index == param.index },
                }).ok_or("caller does not supply the identity parameter")?;
                let caller_param = (m.id == module.id).then(|| params.get(caller)).flatten();
                let caller_f = &m.functions[*caller];
                let source = scene_argument(ws, caller_f, body, arg, text, caller_param, &param.identity)?;
                let span = body.expr_spans[arg];
                let mut range = span.offset_range;
                let replacement = if body.arg_names.contains_key(&arg) {
                    let before = &text[body.expr_spans[e].offset_range.0..range.0];
                    let label = Regex::new(&format!(r"\b{}\s*:\s*$", f.param_decls[param.index].name)).expect("identifier regex");
                    let found = label.find(before).ok_or("unsupported named argument span")?;
                    range.0 = body.expr_spans[e].offset_range.0 + found.start();
                    format!("world: {source}")
                } else { source };
                edits.push(Edit::new(range, replacement));
            }
        }
    }
    Ok(edits)
}

fn scene_argument(
    ws: &Workspace, f: &Function, body: &Body, arg: Idx<Expr>, text: &str,
    helper: Option<&HelperParam>, identity: &str,
) -> std::result::Result<String, String> {
    if let Expr::Var(b) = body.expr_arena[arg] {
        if helper.is_some_and(|p| p.binding == b && p.identity == identity) && !assigned(body, b) {
            return Ok("world".to_string());
        }
    }
    let has_scene = f.param_decls.iter().any(|p| {
        let ty = ws.render(None, p.ty);
        p.name.as_str() == "world" && ty.rsplit_once("Scene<").is_some_and(|(_, inner)| {
            inner.strip_suffix('>').is_some_and(|name| name.rsplit("::").next() == Some("WorldState"))
        })
    });
    if !has_scene && helper.is_none() {
        return Err("caller has no scene parameter to forward".to_string());
    }
    let wanted = if identity == "Player" { "world.player()" } else { "world.get().unwrap()" };
    let resolution = match body.expr_arena[arg] {
        Expr::Var(b) => rebind_text(body, b, text)?,
        _ => slice(text, body.expr_spans[arg]).trim().to_string(),
    };
    if resolution != wanted {
        return Err("caller identity is not resolved from its scene".to_string());
    }
    Ok("world".to_string())
}

fn let_value(body: &Body, binding: Idx<BindingInfo>) -> Option<Idx<Expr>> {
    body.statement_arena.iter().find_map(|(_, s)| match s {
        Statement::Let { name, value } if *name == binding => Some(*value),
        _ => None,
    })
}

/// Stage two (recipe steps 4 and 5): the edits, and the borrows left as
/// written with the reason.
fn rebind_edits(ws: &Workspace, module: &Module, id: Ustr, text: &str) -> (Vec<Edit>, Vec<String>) {
    let bodies: FxHashMap<Idx<Function>, &Body> = functions_in(module, id).map(|(fid, _, body)| (fid, body)).collect();
    let mut crossed: BTreeMap<(Idx<Function>, Idx<BindingInfo>), Vec<Idx<Expr>>> = BTreeMap::new();
    for c in identity_suspension_crossings(ws, module) {
        if bodies.contains_key(&c.function) {
            crossed.entry((c.function, c.binding)).or_default().extend(c.suspensions);
        }
    }
    let mut inserts: BTreeMap<usize, Vec<(usize, String)>> = BTreeMap::new();
    let mut edits = Vec::new();
    let mut left = Vec::new();
    for ((fid, binding), suspensions) in crossed {
        let body = bodies[&fid];
        let at = rebind_text(body, binding, text).and_then(|rebind| {
            let mut lines = Vec::new();
            for s in suspensions {
                lines.push((after_statement(body, s, text)?, rebind.clone()));
            }
            Ok(lines)
        });
        match at {
            Ok(lines) => {
                let name = body.binding_arena[binding].name;
                let decl = body.binding_spans[binding].offset_range.0;
                for ((pos, indent), rebind) in lines {
                    let line = format!("\n{indent}{name} = {rebind}");
                    let at = inserts.entry(pos).or_default();
                    if !at.iter().any(|(_, l)| *l == line) {
                        at.push((decl, line));
                    }
                }
            }
            Err(why) => left.push(left_note(body, binding, text, &why)),
        }
    }
    for (pos, mut lines) in inserts {
        lines.sort();
        edits.push(Edit::new((pos, pos), lines.into_iter().map(|(_, l)| l).collect::<String>()));
    }

    let mut closures: BTreeMap<(Idx<Function>, Idx<Expr>, Idx<BindingInfo>), ()> = BTreeMap::new();
    for site in identity_module_body_sites(ws, module) {
        if site.kind != IdentityStorageKind::Capture || site.span.file != id {
            continue;
        }
        for (&fid, body) in &bodies {
            let use_at = body.expr_arena.iter().find_map(|(e, expr)| match expr {
                Expr::Var(b) if body.expr_spans[e].offset_range == site.span.offset_range => Some(*b),
                _ => None,
            });
            let lambda = innermost(body, site.span, |e| matches!(e, Expr::Lambda { .. }));
            if let (Some(binding), Some(lambda)) = (use_at, lambda) {
                closures.insert((fid, lambda, binding), ());
            }
        }
    }
    for (fid, lambda, binding) in closures.into_keys() {
        let body = bodies[&fid];
        match rebind_text(body, binding, text) {
            Ok(rebind) => {
                let within = body.expr_spans[lambda];
                for (e, expr) in body.expr_arena.iter() {
                    if matches!(expr, Expr::Var(b) if *b == binding) && contains(within, body.expr_spans[e]) {
                        edits.push(Edit::new(body.expr_spans[e].offset_range, rebind.clone()));
                    }
                }
            }
            Err(why) => {
                let note = left_note(body, binding, text, &format!("captured by a closure; {why}"));
                if !left.contains(&note) {
                    left.push(note);
                }
            }
        }
    }
    (edits, left)
}

fn left_note(body: &Body, binding: Idx<BindingInfo>, text: &str, why: &str) -> String {
    let span = body.binding_spans[binding];
    let init = let_value(body, binding).map(|v| slice(text, body.expr_spans[v]).trim().to_string());
    format!(
        "left `{}` (line {}, init `{}`): {why}",
        body.binding_arena[binding].name,
        span.line_range.0 + 1,
        init.as_deref().unwrap_or("-")
    )
}

/// The resolution a rebinding repeats: the `let` initializer when it resolves
/// a handle held in a parameter or an unreassigned local.
fn rebind_text(body: &Body, binding: Idx<BindingInfo>, text: &str) -> std::result::Result<String, String> {
    let resolution = Regex::new(r"^([A-Za-z_]\w*)\.(player\(\)|get\(\)\.unwrap\(\)|get\(\) \?\? return\b.*)$").expect("static regex");
    let value = let_value(body, binding).ok_or("not bound by a plain `let`")?;
    let span = body.expr_spans[value];
    let init = slice(text, span).trim();
    let root = body
        .expr_arena
        .iter()
        .filter(|(e, _)| body.expr_spans[*e].offset_range.0 == span.offset_range.0)
        .min_by_key(|(e, _)| body.expr_spans[*e].offset_range.1)
        .map(|(_, expr)| expr);
    if let Some(Expr::Global(_)) = root {
        let name = init.split(|c: char| !(c.is_alphanumeric() || c == '_')).next().unwrap_or(init);
        return Err(format!("rooted in the global `{name}`; re-reading it could swap the object (A2)"));
    }
    let m = resolution.captures(init).ok_or("the initializer is not a handle resolution")?;
    match root {
        Some(Expr::Var(r)) if body.binding_arena[*r].name.as_str() == &m[1] => {
            if body.binding_arena[*r].kind != BindingKind::Param && assigned(body, *r) {
                return Err(format!("its handle `{}` is reassigned", &m[1]));
            }
        }
        _ => return Err(format!("`{}` is not a parameter or local", &m[1])),
    }
    let death_test = Regex::new(&format!(r"\b{}\.(get\(\)\s*[!=]=\s*null|is_live\(\))", &m[1])).expect("escaped name");
    if death_test.is_match(slice(text, body.expr_spans[body.root.ok_or("no body")?])) {
        return Err(format!("the sim tests `{}` for death, so a rebinding could fail; read what it needs before the suspension", &m[1]));
    }
    let rebind = format!("{}.{}", &m[1], if &m[2] == "player()" { "player()" } else { "get().unwrap()" });
    let other_write = body.expr_arena.iter().any(|(_, expr)| match expr {
        Expr::Assign { target, value } => {
            matches!(body.expr_arena[*target], Expr::Var(b) if b == binding) && slice(text, body.expr_spans[*value]).trim() != rebind
        }
        _ => false,
    });
    if other_write {
        return Err("it is reassigned".to_string());
    }
    Ok(rebind)
}

fn assigned(body: &Body, binding: Idx<BindingInfo>) -> bool {
    body.expr_arena.iter().any(|(_, expr)| {
        matches!(expr, Expr::Assign { target, .. } if matches!(body.expr_arena[*target], Expr::Var(b) if b == binding))
    })
}

fn innermost(body: &Body, span: Span, pick: impl Fn(&Expr) -> bool) -> Option<Idx<Expr>> {
    body.expr_arena
        .iter()
        .filter(|(e, expr)| pick(expr) && contains(body.expr_spans[*e], span))
        .min_by_key(|(e, _)| {
            let (s, t) = body.expr_spans[*e].offset_range;
            t - s
        })
        .map(|(e, _)| e)
}

/// Where a rebinding after `suspension` goes: the end of the line that ends
/// the statement holding it, with that statement's indentation.
fn after_statement(body: &Body, suspension: Idx<Expr>, text: &str) -> std::result::Result<(usize, String), String> {
    let span = body.expr_spans[suspension];
    let block = innermost(body, span, |e| matches!(e, Expr::Block { .. })).ok_or("suspension outside a block")?;
    let Expr::Block { statements, tail } = &body.expr_arena[block] else { unreachable!() };
    let holder = statements
        .clone()
        .map(|s| body.statement_spans[s])
        .chain(tail.map(|t| body.expr_spans[t]))
        .find(|s| contains(*s, span))
        .ok_or("suspension outside a statement")?;
    let (start, end) = holder.offset_range;
    let line_end = text[end..].find('\n').map_or(text.len(), |i| end + i);
    let rest = text[end..line_end].trim();
    if !(rest.is_empty() || rest.starts_with("//")) {
        return Err(format!("line {} continues after its suspension", holder.line_range.1 + 1));
    }
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let indent: String = text[line_start..start].chars().take_while(|c| c.is_whitespace()).collect();
    Ok((line_end, indent))
}

/// Across-yield and capture sites in `id`, unique by line and identity as the
/// census counts them.
fn site_counts(ws: &Workspace, id: Ustr) -> [usize; 3] {
    let Some(module) = module_of(ws, id) else { return [0, 0, 0] };
    let mut seen = std::collections::BTreeSet::new();
    for site in identity_module_body_sites(ws, module) {
        let k = match site.kind {
            IdentityStorageKind::AcrossYield => 0,
            IdentityStorageKind::Capture => 1,
            IdentityStorageKind::CoParam => 2,
            _ => continue,
        };
        if site.span.file == id {
            seen.insert((k, site.span.line_range.0, site.identity));
        }
    }
    let mut out = [0, 0, 0];
    for (k, ..) in seen {
        out[k] += 1;
    }
    out
}
