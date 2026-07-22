//! `rock lsp inline-lua-census <path>`: per-file counts of `inline(lua)` fn
//! bodies (meaningful Lua lines), so raw-Lua debt can be tracked and ratcheted
//! DOWN over time. Purely syntactic -- it parses each file with `skull_parser`
//! rather than typechecking, so it runs on any tree and never drops a fn to an
//! unrelated type error. The body text comes straight from the parser AST
//! (`FunctionBody::Lua`), which keeps it verbatim.

use anyhow::Result;
use skull_parser::{FunctionBody, ItemKind};
use ustr::Ustr;

/// One `inline(lua)` fn: its name and its body's meaningful-line count.
#[derive(Debug, Clone)]
pub struct FnStat {
    pub name: String,
    pub lines: usize,
}

/// One file's inline(lua) fns plus the file's meaningful-line total.
#[derive(Debug, Clone)]
pub struct FileStat {
    /// Display path (relative to cwd when under it, else absolute).
    pub file: String,
    pub fns: Vec<FnStat>,
    pub total_lines: usize,
}

/// The whole census: every file that has any inline(lua) fn, plus grand totals.
#[derive(Debug, Clone, Default)]
pub struct Census {
    pub files: Vec<FileStat>,
    pub total_fns: usize,
    pub total_lines: usize,
}

/// Meaningful = non-blank, non-pure-`--`-comment lines. Same definition the old
/// `LongInlineLuaLintPass` used (`84f5533fa`): a trailing `--` after code still
/// counts the whole line; only a line whose trimmed text *starts* with `--` is
/// dropped, along with blank lines.
fn count_meaningful_lines(src: &str) -> usize {
    src.lines()
        .filter(|line| {
            let t = line.trim();
            !t.is_empty() && !t.starts_with("--")
        })
        .count()
}

/// `path` is a file or directory. `ignores` are plain substrings: a match
/// against a file's display path drops the whole file; a match against a fn
/// name drops that fn. Files with no surviving inline(lua) fn are omitted.
pub fn census_report(path: &str, ignores: &[String]) -> Result<Census> {
    let files = if fs_err::metadata(path).map(|m| m.is_dir()).unwrap_or(false) {
        rock_host::project::find_rock_files_at_path(path)?
    } else {
        vec![path.to_string()]
    };

    let cwd = std::env::current_dir().ok();
    let mut census = Census::default();
    for file in files {
        let disp = display_path(&file, cwd.as_deref());
        if ignores.iter().any(|ig| disp.contains(ig.as_str())) {
            continue;
        }
        let Ok(src) = fs_err::read_to_string(&file) else { continue };
        let Some(file_stat) = file_stat(&disp, &src, ignores) else { continue };
        census.total_fns += file_stat.fns.len();
        census.total_lines += file_stat.total_lines;
        census.files.push(file_stat);
    }
    Ok(census)
}

/// Parse one file's source and collect its inline(lua) fns (fn-name ignores
/// applied). Returns `None` when nothing survives, so the file is omitted.
fn file_stat(disp: &str, src: &str, ignores: &[String]) -> Option<FileStat> {
    let ast = skull_parser::parse_module(src, Ustr::from(disp)).ok()?;
    let mut fns: Vec<FnStat> = Vec::new();
    for item in &ast.items {
        let ItemKind::Function(func) = &item.kind else { continue };
        let FunctionBody::Lua(body) = &func.body else { continue };
        let name = func.name.name.as_str();
        if ignores.iter().any(|ig| name.contains(ig.as_str())) {
            continue;
        }
        fns.push(FnStat { name: name.to_string(), lines: count_meaningful_lines(body) });
    }
    if fns.is_empty() {
        return None;
    }
    fns.sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.name.cmp(&b.name)));
    let total_lines = fns.iter().map(|s| s.lines).sum();
    Some(FileStat { file: disp.to_string(), fns, total_lines })
}

/// Path relative to `cwd` when the file lives under it, else the path as given.
/// Keeps output diffable across machines (repo-relative when run from root).
fn display_path(file: &str, cwd: Option<&std::path::Path>) -> String {
    if let Some(cwd) = cwd {
        if let Ok(rel) = std::path::Path::new(file).strip_prefix(cwd) {
            return rel.to_string_lossy().to_string();
        }
    }
    file.to_string()
}

/// Render the census. `summary` prints ONLY the TOTAL line.
pub fn render(census: &Census, summary: bool) -> String {
    let total = format!(
        "TOTAL: {} fns, {} meaningful lines across {} files",
        census.total_fns,
        census.total_lines,
        census.files.len()
    );
    if summary {
        return format!("{total}\n");
    }
    let mut out = String::new();
    for f in &census.files {
        out.push_str(&f.file);
        out.push('\n');
        let width = f.fns.iter().map(|s| s.name.len()).max().unwrap_or(0).max(1);
        for s in &f.fns {
            out.push_str(&format!("  {:<width$} {:>4} lines\n", s.name, s.lines, width = width));
        }
        out.push_str(&format!(
            "  ({} fns, {} meaningful lines)\n\n",
            f.fns.len(),
            f.total_lines
        ));
    }
    out.push_str(&total);
    out.push('\n');
    out
}

/// CLI handler. Multiple paths are censused into one report (one grand TOTAL),
/// so the whole standard surface yields a single trend number. Exit is always 0
/// (a census, not a gate).
pub fn inline_lua_census(paths: &[String], ignores: &[String], summary: bool) -> Result<()> {
    let mut census = Census::default();
    for path in paths {
        let part = census_report(path, ignores)?;
        census.total_fns += part.total_fns;
        census.total_lines += part.total_lines;
        census.files.extend(part.files);
    }
    print!("{}", render(&census, summary));
    Ok(())
}
