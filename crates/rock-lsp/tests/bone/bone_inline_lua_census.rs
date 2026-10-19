//! `rock lsp inline-lua-census <path>`: per-file inline(lua) body stats.
//! Covers meaningful-line counting (blank/comment exclusion), the descending
//! sort, `--ignore` (by path and by fn name), and the `--summary` render.

use rock_lsp::cli::inline_lua_census::{census_report, render};

/// A file with two inline(lua) fns: `big` has blank + `--`-comment lines that
/// must NOT count; `small` is a one-liner. A plain Rock fn must be ignored.
const SRC: &str = "\
inline(lua) fn big(a: int) -> int = \"\"\"\n\
  local x = a + 1\n\
\n\
  -- a pure lua comment line, excluded\n\
  local y = x * 2  -- trailing comment still counts this line\n\
  return y\n\
\"\"\"\n\
inline(lua) fn small() -> int = \"\"\"return 7\"\"\"\n\
fn plain() -> int { 1 }\n";

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rock"), SRC).unwrap();
    dir
}

#[test]
fn counts_meaningful_lines_excluding_blank_and_comment() {
    let dir = fixture();
    let c = census_report(dir.path().to_str().unwrap(), &[]).unwrap();

    assert_eq!(c.files.len(), 1, "one file with inline(lua) fns");
    let file = &c.files[0];
    // Sorted by line count descending, then name -> big before small.
    assert_eq!(file.fns[0].name, "big");
    assert_eq!(file.fns[1].name, "small");
    // big body: `local x`, `local y`, `return y` meaningful; blank + `--` line excluded.
    assert_eq!(file.fns[0].lines, 3, "blank + pure-comment excluded, trailing-comment line kept");
    assert_eq!(file.fns[1].lines, 1);

    assert_eq!(c.total_fns, 2, "plain Rock fn not counted");
    assert_eq!(c.total_lines, 4);
    assert_eq!(file.total_lines, 4);
}

#[test]
fn ignore_by_path_drops_whole_file() {
    let dir = fixture();
    let c = census_report(dir.path().to_str().unwrap(), &["a.rock".to_string()]).unwrap();
    assert!(c.files.is_empty(), "path-matching ignore removes the file");
    assert_eq!(c.total_fns, 0);
}

#[test]
fn ignore_by_fn_name_drops_that_fn() {
    let dir = fixture();
    let c = census_report(dir.path().to_str().unwrap(), &["big".to_string()]).unwrap();
    assert_eq!(c.total_fns, 1, "only `big` dropped");
    assert_eq!(c.files[0].fns.len(), 1);
    assert_eq!(c.files[0].fns[0].name, "small");
}

#[test]
fn summary_renders_only_total_line() {
    let dir = fixture();
    let c = census_report(dir.path().to_str().unwrap(), &[]).unwrap();
    let out = render(&c, true);
    assert_eq!(out, "TOTAL: 2 fns, 4 meaningful lines across 1 files\n");

    // Full render carries the per-fn detail AND the total.
    let full = render(&c, false);
    assert!(full.contains("big"), "full render lists fns: {full}");
    assert!(full.contains("TOTAL: 2 fns, 4 meaningful lines across 1 files"), "{full}");
}

#[test]
fn unreadable_and_unparsable_files_fail_the_census_by_name() {
    let dir = fixture();
    std::fs::write(dir.path().join("bad_utf8.rock"), b"fn f() {}\n\xff\n").unwrap();
    std::fs::write(dir.path().join("bad_parse.rock"), "fn (((\n").unwrap();
    let path = dir.path().to_str().unwrap().to_string();
    let err = rock_lsp::cli::inline_lua_census::inline_lua_census(&[path], &[], true)
        .expect_err("census must fail when files are left out")
        .to_string();
    assert!(err.contains("2 files") && err.contains("bad_utf8.rock") && err.contains("bad_parse.rock"), "{err}");
}
