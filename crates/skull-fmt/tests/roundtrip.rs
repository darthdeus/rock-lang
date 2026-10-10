//! Real-source guardrails for the LIVE formatter (`rock fmt` routes through
//! `skull_fmt::format_code`). Over every `.rock` file in stdlib + games + stone
//! + mag, formatting must:
//!   1. be idempotent -- `format(format(x)) == format(x)` (a fixed point), and
//!   2. reparse cleanly -- the output is valid Rock, and
//!   3. neither invent nor drop comments -- the output's comment multiset
//!      EQUALS the source's (catches the duplicate-trailing-comment class AND
//!      the standalone-comment-loss class).
//!
//! These ran ad-hoc while a round of real-file corruption bugs was fixed
//! (byte-vs-char slicing, operator bodies, blank/param oscillation, an extra `>`
//! that produced unparseable output, trailing-comment loss/dup); this pins them.

use rock_core::source_code::Dialect;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("rock") {
            out.push(p);
        }
    }
}

fn rock_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in ["stdlib", "games", "stone", "mag"] {
        collect(&repo_root().join(dir), &mut out);
    }
    out.sort();
    out
}

/// Multiset of `//...` comment suffixes per line (trailing whitespace trimmed).
/// `//` inside a verbatim string/lua body appears identically in both source and
/// output, so it cancels in the subset check.
fn comment_multiset(src: &str) -> std::collections::HashMap<String, usize> {
    let mut m = std::collections::HashMap::new();
    for line in src.lines() {
        if let Some(idx) = line.find("//") {
            *m.entry(line[idx..].trim_end().to_string()).or_default() += 1;
        }
    }
    // Block comments (`/* ... */`), whole-slice keyed so `/* x */` and `// x`
    // never alias. A `/*` inside a `//` line comment or a string round-trips
    // verbatim, so it cancels in the equality check.
    let mut rest = src;
    while let Some(a) = rest.find("/*") {
        if let Some(b) = rest[a..].find("*/") {
            *m.entry(rest[a..a + b + 2].to_string()).or_default() += 1;
            rest = &rest[a + b + 2..];
        } else {
            break;
        }
    }
    m
}

/// Multiset of `@name` markers, counted off the TOKENS rather than the text so
/// an `@` inside a string or an `inline(lua)` body cannot be mistaken for one.
/// Covers every position at once -- item, fn parameter, struct and
/// record-variant field -- which makes it a guard on positions nobody has
/// added yet, and lets it see a marker the PARSER drops rather than only one the printer forgets.
///
/// Idempotency cannot see a dropped marker: the stripped form reparses and
/// re-formats to itself, so both sides agree and the gate stays green. That is
/// how parameter annotations can be deleted silently.
///
/// `@` is ALSO the `wait_for` condition operator (`yield wait_for 5000 @ {
/// cond }`), so the sigil is identified by what precedes it rather than by the
/// `@name` shape alone: an annotation opens its line or follows `(`, `,` or
/// `{`, while the operator always follows an expression. Without this the
/// formatter's `@ { player.kills == 1 }` -> `@ player.kills == 1` rewrite reads
/// as an invented `@player`.
fn annotation_multiset(src: &str) -> std::collections::HashMap<String, usize> {
    use rock_tokenizer::TokenKind as T;
    let mut m = std::collections::HashMap::new();
    let Ok(tokens) = rock_tokenizer::Tokenizer::new(src, "<anno>".into()).tokenize() else {
        return m;
    };
    // Index of the token just past the last annotation counted, so a RUN of
    // them (`@scalar_abi @nomangle inline(js) fn ..`, and the same run split
    // across lines, which the formatter joins) keeps counting after the first.
    let mut run_end: Option<usize> = None;
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].kind != T::At {
            i += 1;
            continue;
        }
        let opens_position = run_end == Some(i)
            || match i.checked_sub(1).map(|p| &tokens[p]) {
                None => true,
                Some(prev) => {
                    prev.span.line_range.1 != tokens[i].span.line_range.0
                        || matches!(prev.kind, T::OpenParen | T::Comma | T::OpenCurly)
                }
            };
        if !opens_position {
            i += 1;
            continue;
        }
        let Some(T::Ident(name)) = tokens.get(i + 1).map(|t| &t.kind) else {
            i += 1;
            continue;
        };
        *m.entry(name.as_str().to_string()).or_default() += 1;
        // Step past `@name` and any balanced `(args)` so the next token is
        // where a sibling annotation would start.
        let mut j = i + 2;
        if tokens.get(j).map(|t| &t.kind) == Some(&T::OpenParen) {
            let mut depth = 0usize;
            while j < tokens.len() {
                match tokens[j].kind {
                    T::OpenParen => depth += 1,
                    T::CloseParen => {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        run_end = Some(j);
        i = j;
    }
    m
}

#[test]
fn real_sources_keep_every_annotation_marker() {
    let files = rock_files();
    assert!(files.len() > 100, "expected the real corpus, found {}", files.len());

    // The guard is only evidence if the corpus actually CARRIES the positions
    // it is meant to protect. Assert the population, not just the absence of a
    // difference: a zero here would make every arm below vacuous.
    let mut total: usize = 0;
    let mut fails = Vec::new();
    for f in &files {
        let short = f.strip_prefix(repo_root()).unwrap_or(f).display();
        let Ok(src) = fs::read_to_string(f) else { continue };
        let out = skull_fmt::format_code(&src);

        let src_a = annotation_multiset(&src);
        let out_a = annotation_multiset(&out);
        total += src_a.values().sum::<usize>();

        for (name, n) in &src_a {
            let got = out_a.get(name).copied().unwrap_or(0);
            if got < *n {
                fails.push(format!("@{name} dropped in {short}: {n} -> {got}"));
            }
        }
        for (name, n) in &out_a {
            let had = src_a.get(name).copied().unwrap_or(0);
            if *n > had {
                fails.push(format!("@{name} invented in {short}: {had} -> {n}"));
            }
        }
    }

    assert!(fails.is_empty(), "{} annotation drift(s):\n{}", fails.len(), fails.join("\n"));
    assert!(total > 500, "corpus carries only {total} annotations -- the guard is vacuous");
}

#[test]
fn real_sources_are_idempotent_reparse_and_keep_comments() {
    let files = rock_files();
    assert!(
        files.len() > 100,
        "expected the real corpus (>100 files), found {} -- repo layout changed?",
        files.len()
    );

    let mut fails = Vec::new();
    for f in &files {
        let short = f.strip_prefix(repo_root()).unwrap_or(f).display();
        let Ok(src) = fs::read_to_string(f) else { continue };

        let once = skull_fmt::format_code(&src);
        let twice = skull_fmt::format_code(&once);
        if once != twice {
            fails.push(format!("not idempotent: {short}"));
            continue;
        }

        // Only a *changed* output is the formatter's responsibility to keep
        // valid (an unparseable input is passed through unchanged).
        if once != src {
            match skull_parser::parse_module_clean(&once, "<roundtrip>".into()) {
                Ok(ast) if ast.recovery_errors.is_empty() => {}
                Ok(ast) => fails.push(format!(
                    "output has recovery errors: {short}: {:?}",
                    ast.recovery_errors
                )),
                Err(e) => fails.push(format!("output does not parse: {short}: {e:?}")),
            }
        }

        let src_c = comment_multiset(&src);
        let out_c = comment_multiset(&once);
        for (text, n) in &out_c {
            if *n > src_c.get(text).copied().unwrap_or(0) {
                fails.push(format!("comment invented/duplicated in {short}: {text:?}"));
            }
        }
        // ...and drops none: every source comment survives (formatting must
        // preserve comments, not just avoid inventing them).
        for (text, n) in &src_c {
            if *n > out_c.get(text).copied().unwrap_or(0) {
                fails.push(format!("comment dropped in {short}: {text:?}"));
            }
        }
    }

    assert!(
        fails.is_empty(),
        "{} of {} real files failed:\n{}",
        fails.len(),
        files.len(),
        fails.join("\n")
    );
}

#[test]
fn nullable_function_returning_nullable_value_keeps_its_outer_parens() {
    let source = "struct Hooks {\n    callback: (() -> Vec2?)? = null,\n}\n";
    let formatted = skull_fmt::format_code(source);
    let ast = skull_parser::parse_module_clean(&formatted, "<roundtrip>".into())
        .expect("formatted nullable callback must parse");
    assert!(ast.recovery_errors.is_empty(), "{formatted}");
    assert!(formatted.contains("(() -> Vec2?)?"), "{formatted}");
}

#[test]
fn nullable_parenthesized_function_type_roundtrips_as_the_same_ast() {
    let source = "struct Hooks {\n    on_close: (fn())? = null,\n}\n";
    let source_ast = skull_parser::parse_module_clean(source, "<source>".into())
        .expect("the nullable parenthesized function type must parse");
    let formatted = skull_fmt::format_code(source);
    let formatted_ast = skull_parser::parse_module_clean(&formatted, "<formatted>".into())
        .expect("formatted output must parse");

    assert_eq!(
        source_ast.module_sexp(),
        formatted_ast.module_sexp(),
        "formatting `(fn())?` must preserve its nullable function-type AST; output was:\n{formatted}"
    );
    assert_eq!(formatted.trim(), "struct Hooks {\n    on_close: (fn() -> ())? = null,\n}");
}

/// Modules-v1 forms round-trip under the v1 dialect: format(format(x)) == f
/// format(x), the output reparses clean, and — the load-bearing check — a
/// v1 ascription prints as `(expr : Type)`, which still parses in v1
/// (an infix re-print would not).
#[test]
fn modules_v1_forms_roundtrip() {
    let sources = [
        "fn f() {\n    let x = (5 : int)\n    let y = (x : float) / 2\n    print(y)\n}\n",
        "fn f() {\n    let g = ((v => v + 1) : fn(int) -> int)\n    print(g(1))\n}\n",
        "fn f() {\n    let t = ((1, 2) : (int, int))\n    print(t)\n}\n",
        "struct S {\n    name: string,\n    private(set) health: float = 100,\n    pub field: int,\n}\n",
        "pub mod combat {\n    pub fn heal() {}\n}\n",
        "use a::b::*;\nuse a::b::{Actor, heal as restore};\nuse geo::operator(+);\n\nfn f() {}\n",
        "pub @inline fn a() -> int { 1 }\n\npub(package) @pure\nfn b() -> int {\n    2\n}\n",
    ];
    for src in sources {
        let cfg = skull_fmt::FormatConfig::default();
        let once = skull_fmt::try_format_with_dialect(src, &cfg, Dialect::V1)
            .unwrap_or_else(|e| panic!("format failed for {src:?}: {e}"));
        let twice = skull_fmt::try_format_with_dialect(&once, &cfg, Dialect::V1)
            .unwrap_or_else(|e| panic!("reformat failed for {src:?}: {e}"));
        assert_eq!(once, twice, "format must be a fixed point for {src:?}");
        // The formatted output must reparse clean under v1.
        let ast = skull_parser::parse_module_with_dialect(
            &once,
            ustr::Ustr::from("rt.rock"),
            Dialect::V1,
        )
        .expect("formatted output must parse");
        assert!(
            ast.recovery_errors.is_empty(),
            "formatted output has recovery errors: {:?}\n{}",
            ast.recovery_errors,
            once
        );
    }
}

/// A visibility ahead of an item's annotations survives formatting.
#[test]
fn v1_visibility_on_an_annotated_item_is_kept() {
    let cfg = skull_fmt::FormatConfig::default();
    let src = "pub @inline fn a() -> int { 1 }\n\npub(package) @pure\nfn b() -> int {\n    2\n}\n";
    let plain = "pub fn c() -> int {\n    3\n}\n\npub struct S {\n    x: int,\n    pub(package) y: int,\n    pub private(set) z: int = 0,\n}\n\nprivate global G: int = 1\n\npub(in a::b) @inline inline(lua) fn d() -> int = \"\"\"return 1\"\"\"\n";
    assert_eq!(skull_fmt::try_format_with_dialect(plain, &cfg, Dialect::V1).expect("format"), plain);
    let out = skull_fmt::try_format_with_dialect(src, &cfg, Dialect::V1).expect("format");
    assert_eq!(out, src);
}

/// A v1 ascription must NOT print as the infix `::` form: that would not
/// reparse in v1.
#[test]
fn v1_ascription_prints_parenthesized() {
    let src = "fn f() {\n    let x = (5 : int)\n    print(x)\n}\n";
    let cfg = skull_fmt::FormatConfig::default();
    let out = skull_fmt::try_format_with_dialect(src, &cfg, Dialect::V1).expect("format");
    assert!(out.contains("(5 : int)"), "expected parenthesized spelling, got:\n{out}");
    assert!(!out.contains("5 :: int"), "infix spelling leaked into v1 output:\n{out}");
}
