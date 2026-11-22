//! The type-checking front end shared by every command.

use crate::Result;
use bone_core::prelude::Instant;
use rock_core::CompilerError;
use rock_core::SourceFiles;
use rock_core::source_file_from_path_and_contents;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Copy, Default)]
pub(crate) struct BonePhaseTimes {
    pub parse: Duration,
    pub semantic: Duration,
    pub stdlib_assemble: Duration,
    pub semantic_call: Duration,
    semantic_pass_called: bool,
}

impl BonePhaseTimes {
    pub fn total(&self) -> Duration {
        self.parse + self.semantic
    }

    fn add(&mut self, other: &BonePhaseTimes) {
        self.parse += other.parse;
        self.semantic += other.semantic;
        self.stdlib_assemble += other.stdlib_assemble;
        self.semantic_call += other.semantic_call;
        self.semantic_pass_called |= other.semantic_pass_called;
    }
}

pub(crate) fn body_errors(ws: &bone_semantic::inference::Workspace) -> Vec<CompilerError> {
    skull_mir::verdict::body_errors(ws)
}

pub(crate) fn render_errs(sources: &SourceFiles, errs: &[CompilerError]) -> String {
    // A diagnostic can land in an INLINED stdlib body, whose modules are parsed
    // under the loader's stdlib id and appear in no caller's source set -- the
    // span lookup then fails and the snippet plus every label is dropped. Added
    // here, the one sink all render paths share, rather than per caller.
    let mut sf = sources.clone();
    for f in rock_stdlib::stdlib_files() {
        let id = rock_stdlib::stdlib_file_id(f.name);
        sf.add_or_update_file(source_file_from_path_and_contents(
            id.clone(),
            id,
            f.contents.to_string(),
        ));
    }
    let expanded: Vec<CompilerError> = errs
        .iter()
        .flat_map(|e| {
            std::iter::once(CompilerError {
                siblings: Vec::new(),
                ..e.clone()
            })
            .chain(e.siblings.iter().cloned())
        })
        .collect();
    rock_core::errors::render_all(&expanded, &sf)
}

pub(crate) fn aliases_for_modules<'a>(
    modules: impl IntoIterator<Item = &'a skull_parser::Ast>,
    aliases: Option<&bone_core::modules::NsAliases>,
) -> std::result::Result<bone_core::modules::NsAliases, CompilerError> {
    rock_host::program_inputs::ns_aliases_for_selected_sources(
        modules
            .into_iter()
            .map(|m| (m.file.as_str(), m.package.as_ref())),
        aliases,
    )
}

pub fn check_modules(
    modules: &mut skull_parser::stdlib::ModuleSet,
) -> std::result::Result<bone_semantic::inference::Workspace, Vec<CompilerError>> {
    check_modules_with_sources(modules, None)
}

pub(crate) fn check_modules_with_sources(
    modules: &mut skull_parser::stdlib::ModuleSet,
    sources: Option<&SourceFiles>,
) -> std::result::Result<bone_semantic::inference::Workspace, Vec<CompilerError>> {
    let (ws, errs) = skull::pipeline::check_modules(modules, |modules| {
        aliases_for_modules(modules.iter(), sources.and_then(|s| s.ns_aliases.as_ref()))
    })?;
    if !errs.is_empty() {
        return Err(errs);
    }
    Ok(ws)
}

pub fn check_with_stdlib(
    user: impl IntoIterator<Item = skull_parser::Ast>,
) -> std::result::Result<bone_semantic::inference::Workspace, Vec<CompilerError>> {
    check_modules(&mut skull::pipeline::with_stdlib(user).map_err(|e| vec![e])?)
}

pub(crate) fn bone_session(
    sources: &SourceFiles,
) -> std::result::Result<bone_incremental::session::Session, CompilerError> {
    let stdlib = skull_parser::stdlib::cached()
        .as_ref()
        .map_err(Clone::clone)?;
    bone_incremental::session::Session::new(
        stdlib.clone(),
        session_files(sources),
        sources.ns_aliases.as_ref().unwrap_or(&Default::default()),
    )
}

pub(crate) fn session_files(sources: &SourceFiles) -> Vec<bone_incremental::session::SessionFile> {
    sources
        .files
        .iter()
        .map(|source| bone_incremental::session::SessionFile::from_source(&source.borrow()))
        .collect()
}

pub(crate) fn bone_session_for_emission(
    sources: &SourceFiles,
    anchor: &std::path::Path,
) -> std::result::Result<bone_incremental::session::Session, CompilerError> {
    let mut session = bone_session(sources)?;
    let inputs = crate::comptime::inputs_for_entry_path(anchor)
        .and_then(|inputs| {
            crate::comptime::resolve_checked_inputs(&session.ws, inputs, |file| {
                session.files.get(file).map(|source| source.text.clone())
            })
        })
        .map_err(|e| {
            CompilerError::new_no_location(format!("build var resolution failed: {e:#}"))
        })?;
    session.ws.comptime_inputs = inputs;
    Ok(session)
}

fn bone_pipeline_timed(
    src: &str,
    path: &str,
) -> (std::result::Result<(), Vec<CompilerError>>, BonePhaseTimes) {
    let mut times = BonePhaseTimes::default();

    let t = Instant::now();
    let module = match skull_parser::parse_module(src, ustr::Ustr::from(path)) {
        Ok(m) => m,
        Err(e) => {
            times.parse = t.elapsed();
            return (Err(vec![e]), times);
        }
    };
    times.parse = t.elapsed();

    let t = Instant::now();
    let file = module.file;
    let res = check_with_stdlib([module]).and_then(|ws| {
        check_noalloc(ws, file, None)
            .map(|_| ())
            .map_err(|error| vec![error])
    });
    times.semantic = t.elapsed();
    (res, times)
}

pub(crate) fn parse_user_modules(
    sources: &SourceFiles,
) -> std::result::Result<(Vec<skull_parser::Ast>, ustr::Ustr, Duration), Vec<CompilerError>> {
    let t = Instant::now();
    let parsed: Vec<_> = parse_user_files(sources, |source, file| {
        skull_parser::parse_module(source, file).map(|module| (module, ()))
    })?
    .into_iter()
    .map(|(module, ())| module)
    .collect();
    let entry = project_entry(&parsed)?;
    Ok((parsed, entry, t.elapsed()))
}

pub(crate) fn parse_user_files<T: Send>(
    sources: &SourceFiles,
    parse: impl Fn(&str, ustr::Ustr) -> std::result::Result<(skull_parser::Ast, T), CompilerError>
    + Sync,
) -> std::result::Result<Vec<(skull_parser::Ast, T)>, Vec<CompilerError>> {
    bone_core::prelude::par_map_largest_first(
        sources.files.iter().collect(),
        |file| file.borrow().contents.len(),
        |file| {
            let f = file.borrow();
            let id = ustr::Ustr::from(f.file.to_id_path().as_str());
            let (mut module, facts) = parse(&f.contents, id).map_err(|error| vec![error])?;
            module.package = f.package.clone();
            module.global_policy = f.global_policy;
            Ok((module, facts))
        },
    )
    .into_iter()
    .collect()
}

pub(crate) fn project_entry(
    parsed: &[skull_parser::Ast],
) -> std::result::Result<ustr::Ustr, Vec<CompilerError>> {
    let mut user_file: Option<ustr::Ustr> = None;
    let mut first_file: Option<ustr::Ustr> = None;
    let mut seen = HashSet::with_capacity(parsed.len());
    for module in parsed {
        let mfile = module.file;
        if !seen.insert(mfile) {
            return Err(vec![CompilerError::new_no_location(format!(
                "bone: duplicate module id `{}` in project assembly (also `{}`)",
                mfile, mfile
            ))]);
        }
        if first_file.is_none() {
            first_file = Some(mfile);
        }
        let ids = mfile.as_str();
        if ids.ends_with("/main.rock") || ids.ends_with("\\main.rock") || ids == "main.rock" {
            user_file = Some(mfile);
        }
    }

    let syntax = skull::pipeline::syntax_errors(parsed);
    if !syntax.is_empty() {
        return Err(syntax);
    }

    user_file.or(first_file).ok_or_else(|| {
        vec![CompilerError::new_no_location(
            "bone: project has no .rock files",
        )]
    })
}

pub fn entry_module_id(
    ws: &bone_semantic::inference::Workspace,
    file: ustr::Ustr,
) -> bone_semantic::prelude::ModuleId {
    ws.module_ids[&file]
}

pub(crate) fn workspace_frontend_timed(
    sources: &SourceFiles,
) -> (
    std::result::Result<(bone_semantic::inference::Workspace, ustr::Ustr), Vec<CompilerError>>,
    BonePhaseTimes,
) {
    let mut times = BonePhaseTimes::default();
    let (user_modules, entry_file, parse) = match parse_user_modules(sources) {
        Ok(parsed) => parsed,
        Err(errors) => return (Err(errors), times),
    };
    times.parse = parse;

    let t_semantic = Instant::now();
    let workspace = (|| -> std::result::Result<_, CompilerError> {
        let t_assemble = Instant::now();
        let mut modules = skull::pipeline::with_stdlib(user_modules)?;
        times.stdlib_assemble = t_assemble.elapsed();
        let aliases = aliases_for_modules(modules.iter(), sources.ns_aliases.as_ref())?;

        let t_call = Instant::now();
        times.semantic_pass_called = true;
        let workspace = skull::pipeline::semantic_pass(&mut modules, &aliases);
        times.semantic_call = t_call.elapsed();
        workspace.map(|workspace| (workspace, entry_file))
    })();
    times.semantic = t_semantic.elapsed();
    (workspace.map_err(|error| vec![error]), times)
}

pub(crate) fn categorize(msg: &str) -> String {
    let head = |s: &str| -> String {
        s.trim()
            .split(|c: char| c == '{' || c == '(' || c.is_whitespace())
            .find(|t| !t.is_empty())
            .unwrap_or("")
            .to_string()
    };
    let after = |s: &str, marker: &str| -> Option<String> {
        s.split_once(marker).map(|(_, rest)| head(rest))
    };

    if let Some(rest) = msg.strip_prefix("Unsupported top level item") {
        return format!(
            "unsupported top-level item: {}",
            after(rest, "kind: ").unwrap_or_default()
        );
    }
    if let Some(rest) = msg.strip_prefix("Unsupported type expr") {
        return format!("unsupported type expr: {}", head(rest));
    }
    if msg.starts_with("unsupported function body type") {
        return format!(
            "unsupported function body: {}",
            after(msg, "body: ").unwrap_or_default()
        );
    }
    if let Some(name) = msg
        .strip_prefix("Function ")
        .and_then(|r| r.strip_suffix(" doesn't exist"))
    {
        return format!("missing function/operator: {}", name);
    }
    if let Some(rest) = msg.strip_prefix("unsupported statement:") {
        return format!("unsupported statement: {}", head(rest));
    }
    if let Some(rest) = msg.strip_prefix("unsupported expr") {
        return format!("unsupported expr: {}", head(rest));
    }
    if let Some(got) = after(msg, ", got ") {
        return format!("unsupported call form: {}", got);
    }

    let line = msg.lines().next().unwrap_or(msg);
    if line.chars().count() > 70 {
        format!("{}...", line.chars().take(70).collect::<String>())
    } else {
        line.to_string()
    }
}

pub(crate) fn bone_check_single(path: &str, iters: usize) -> Result<()> {
    if Path::new(path).is_dir() {
        return bone_check_project(path, iters);
    }

    let src = match fs_err::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "BONE", "cannot read {}: {}", path, e);
            crate::invocation_log::exit(1);
        }
    };

    let iters = iters.max(1);
    let mut agg = BonePhaseTimes::default();
    let mut first_err: Option<Vec<CompilerError>> = None;
    for _ in 0..iters {
        let (res, times) = bone_pipeline_timed(&src, path);
        agg.add(&times);
        if first_err.is_none() {
            if let Err(errs) = res {
                first_err = Some(errs);
            }
        }
    }

    if let Some(errs) = first_err {
        eprintln!(
            "{}",
            render_errs(&crate::common::single_source(path, &src), &errs)
        );
        crate::invocation_log::exit(1);
    }

    log::info!(target: "BONE", "{} type-checks", path);
    report_phase_timing(path, &agg, iters);
    Ok(())
}

fn bone_check_project(path: &str, iters: usize) -> Result<()> {
    let sources = rock_host::compilation_units::load_sources(std::path::Path::new(path))?;

    let iters = iters.max(1);
    let mut agg = BonePhaseTimes::default();
    let mut last_ws: Option<bone_semantic::inference::Workspace> = None;
    let mut first_err: Option<Vec<CompilerError>> = None;
    for _ in 0..iters {
        let (res, mut times) = workspace_frontend_timed(&sources);
        if !times.semantic_pass_called {
            times.semantic = Duration::ZERO;
        }
        agg.add(&times);
        match res {
            Ok((ws, file)) => {
                let mut errs = body_errors(&ws);
                if errs.is_empty() {
                    match check_noalloc(ws, file, Some(path)) {
                        Ok(ws) => last_ws = Some(ws),
                        Err(error) => errs.push(error),
                    }
                }
                if first_err.is_none() && !errs.is_empty() {
                    first_err = Some(errs);
                }
            }
            Err(errs) => {
                if first_err.is_none() {
                    first_err = Some(errs);
                }
            }
        }
    }

    if let Some(errs) = first_err {
        eprintln!("{}", render_errs(&sources, &errs));
        crate::invocation_log::exit(1);
    }

    log::info!(target: "BONE", "{} type-checks", path);

    let ws = last_ws.expect("a successful iteration must have set last_ws");
    let compile = Duration::from_secs_f64(agg.total().as_secs_f64() / iters as f64);
    log_compile_stats(&sources, &ws, compile, compile, None);

    if iters > 1 {
        report_phase_timing(path, &agg, iters);
    }
    Ok(())
}

pub(crate) fn check_allocation_sites(path: &str, filter: Option<&str>) -> Result<()> {
    use skull::allocation_report;
    use skull::mir_emit::{FullEmit, inspect_allocations};
    use std::io::Write as _;

    let anchor = Path::new(path);
    let sources = if anchor.is_dir() {
        rock_host::compilation_units::load_sources(anchor)?
    } else {
        crate::common::single_source(path, &fs_err::read_to_string(anchor)?)
    };
    let (mut ws, entry) = workspace_frontend_timed(&sources)
        .0
        .map_err(|errors| anyhow::anyhow!("{}", render_errs(&sources, &errors)))?;
    let errors = body_errors(&ws);
    anyhow::ensure!(errors.is_empty(), "{}", render_errs(&sources, &errors));
    crate::comptime::stamp_workspace(&mut ws, anchor)?;

    let _scope = allocation_report::scope(true);
    let (emit, _) = FullEmit::one_shot()
        .project_root(anchor.is_dir().then_some(path))
        .run(&ws, entry_module_id(&ws, entry))
        .map_err(|error| anyhow::anyhow!("{}", render_errs(&sources, &[error])))?;
    let report = emit.allocation_report;
    let files: Vec<_> = ws
        .source_modules()
        .filter_map(|module| {
            let file = module.file.as_str();
            (!file.starts_with('<')).then_some(file)
        })
        .collect();
    let (unpublished, _) = inspect_allocations(&ws, &files, true, Some(&report));
    let mut rows = Vec::new();
    for body in report.bodies.values().chain(&unpublished) {
        let name = skull_mir::mangle::declared_name(&body.diagnostic_name);
        for site in &body.direct_sites {
            let (Some(span), Some(reason)) = (body.source_span(site), site.reason()) else {
                continue;
            };
            let file = span.file.as_str();
            if filter.is_none_or(|filter| {
                filter == name
                    || filter == body.diagnostic_name
                    || filter == body.emitted_name
                    || Path::new(file).ends_with(filter)
            }) {
                rows.push((
                    file,
                    span.line_range().0 + 1,
                    span.offset_range().0,
                    name,
                    reason,
                    site.id,
                ));
            }
        }
    }
    rows.sort_unstable();
    rows.dedup();
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for (file, line, _, name, reason, id) in rows {
        writeln!(out, "{file}:{line}\t{name}\t{reason}\t{id:016x}")?;
    }
    out.flush()?;
    Ok(())
}

fn check_noalloc(
    mut ws: bone_semantic::inference::Workspace,
    file: ustr::Ustr,
    project_root: Option<&str>,
) -> std::result::Result<bone_semantic::inference::Workspace, CompilerError> {
    if skull::mir_emit::noalloc::required(&ws) {
        if let Some(root) = project_root {
            crate::comptime::stamp_workspace(&mut ws, Path::new(root)).map_err(|error| {
                CompilerError::new_no_location(format!("build var resolution failed: {error:#}"))
            })?;
        }
        skull::mir_emit::FullEmit::one_shot()
            .project_root(project_root)
            .run(&ws, entry_module_id(&ws, file))?;
    }
    Ok(ws)
}

fn bone_loc_stats(sources: &SourceFiles) -> rock_core::CompileStats {
    let mut stats = rock_core::CompileStats::default();
    for file in &sources.files {
        let f = file.borrow();
        stats.files += 1;
        stats.total_lines += f.contents.lines().count();
        stats.bytes += f.contents.len();
    }
    for f in rock_stdlib::stdlib_files().iter() {
        stats.files += 1;
        stats.total_lines += f.contents.lines().count();
        stats.bytes += f.contents.len();
    }
    stats
}

pub(crate) fn log_compile_stats(
    sources: &SourceFiles,
    ws: &bone_semantic::inference::Workspace,
    compile: Duration,
    total: Duration,
    output: Option<&str>,
) {
    if !log::log_enabled!(target: "COMPILE", log::Level::Debug) {
        return;
    }
    let stats = bone_loc_stats(sources);
    let symbols: usize = ws
        .modules
        .iter()
        .map(|m| {
            m.functions.len() + m.structs.len() + m.enums.len() + m.newtypes.len() + m.globals.len()
        })
        .sum();
    let compile_ms = compile.as_secs_f64() * 1000.0;
    let total_ms = total.as_secs_f64() * 1000.0;
    match output {
        Some(out) => log::debug!(target: "COMPILE",
            "done in {:.2}ms (compile {:.2}ms, {} symbols, {} lines, {}) -> {}",
            total_ms, compile_ms, symbols, stats.total_lines, stats.format_throughput(compile), out),
        None => log::debug!(target: "COMPILE",
            "done in {:.2}ms (compile {:.2}ms, {} symbols, {} lines, {})",
            total_ms, compile_ms, symbols, stats.total_lines, stats.format_throughput(compile)),
    }
}

fn report_phase_timing(label: &str, agg: &BonePhaseTimes, iters: usize) {
    let iters = iters.max(1);
    let ms = |d: Duration| d.as_secs_f64() * 1000.0 / iters as f64;
    if iters > 1 {
        log::info!(target: "PROFILE",
            "bone {}: {:.3}ms/iter = parse {:.3} + semantic {:.3} (mean over {} iters)",
            label, ms(agg.total()), ms(agg.parse), ms(agg.semantic), iters);
    } else {
        log::info!(target: "PROFILE",
            "bone {}: {:.3}ms = parse {:.3} + semantic {:.3}",
            label, ms(agg.total()), ms(agg.parse), ms(agg.semantic));
    }
}

pub(crate) fn short_path(path: &str) -> String {
    let p = Path::new(path);
    let file = p.file_name().and_then(|s| s.to_str()).unwrap_or(path);
    match p
        .parent()
        .and_then(|d| d.file_name())
        .and_then(|s| s.to_str())
    {
        Some(dir) => format!("{}/{}", dir, file),
        None => file.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diagnostic_spanned_into_stdlib_renders_its_snippet() {
        const NEEDLE: &str = "fn Vec<T>.push(";
        let unified = rock_stdlib::stdlib_files()
            .iter()
            .find(|f| f.name == "unified.rock")
            .expect("unified.rock is a stdlib module");
        let offset = unified
            .contents
            .find(NEEDLE)
            .expect("needle still in unified.rock");
        let head = &unified.contents[..offset];
        let line = head.matches('\n').count();
        let col = offset - head.rfind('\n').map_or(0, |i| i + 1);
        let span = rock_core::Span::new(
            ustr::Ustr::from(rock_stdlib::stdlib_file_id("unified.rock").as_str()),
            (line, line),
            (col, col + NEEDLE.len()),
            (offset, offset + NEEDLE.len()),
        );
        let err = CompilerError::new_with_label(span, "boxed here".to_string());
        let rendered = render_errs(&SourceFiles::empty(), &[err]);

        assert!(
            rendered.contains(NEEDLE),
            "the stdlib source line must render, not be dropped:\n{rendered}"
        );
        assert!(
            rendered.contains("boxed here"),
            "the label must render:\n{rendered}"
        );
    }
}
