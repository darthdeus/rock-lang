//! Seeded token deletions, insertions and swaps on generated programs: a fresh
//! check and compile, a live edit and the editor queries all answer with
//! diagnostics, never a panic.

use super::generator::program;
use bone_query::BoneWorkspace;
use lsp_types::{Position, Range};
use rock_cli::skull_backend::IncrementalProject;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// Byte ranges of identifiers, numbers, string literals and punctuation.
fn tokens(source: &str) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'.';
    let mut spans = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        match bytes[at] {
            b if b.is_ascii_whitespace() => {
                at += 1;
                continue;
            }
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
                continue;
            }
            b'"' => {
                at += 1;
                while at < bytes.len() && bytes[at] != b'"' {
                    at += if bytes[at] == b'\\' { 2 } else { 1 };
                }
                at = (at + 1).min(bytes.len());
            }
            b if word(b) => {
                while at < bytes.len() && word(bytes[at]) {
                    at += 1;
                }
            }
            _ => at += 1,
        }
        spans.push((start, at));
    }
    spans
}

fn position(source: &str, offset: usize) -> Position {
    let before = &source[..offset];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    Position::new(
        before.matches('\n').count() as u32,
        before[line_start..].encode_utf16().count() as u32,
    )
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "non-string panic".into())
}

/// One mutation of `source` and the offset where it happened.
fn mutate(
    source: &str,
    spans: &[(usize, usize)],
    pick: &mut impl FnMut(usize) -> usize,
) -> (String, usize, String) {
    let (start, end) = spans[pick(spans.len())];
    let (from, to) = spans[pick(spans.len())];
    let other = &source[from..to];
    let (replacement, what) = match pick(4) {
        0 => (String::new(), format!("delete `{}`", &source[start..end])),
        1 => (
            format!("{other} {}", &source[start..end]),
            format!("insert `{other}`"),
        ),
        2 => (
            other.to_string(),
            format!("replace `{}` with `{other}`", &source[start..end]),
        ),
        _ => {
            // Swap with the next token, keeping the text between them.
            let next = spans
                .iter()
                .find(|s| s.0 >= end)
                .copied()
                .unwrap_or((end, end));
            let swapped = format!(
                "{}{}{}",
                &source[next.0..next.1],
                &source[end..next.0],
                &source[start..end]
            );
            let mutated = format!("{}{swapped}{}", &source[..start], &source[next.1..]);
            return (
                mutated,
                start,
                format!(
                    "swap `{}` and `{}`",
                    &source[start..end],
                    &source[next.0..next.1]
                ),
            );
        }
    };
    (
        format!("{}{replacement}{}", &source[..start], &source[end..]),
        start,
        what,
    )
}

fn fresh(text: &str) {
    let Ok(ast) = skull_parser::parse_module(text, ustr::Ustr::from("mutated.rock")) else {
        return;
    };
    if let Ok(ws) = rock_cli::bone_backend::check_with_stdlib([ast])
        && let Ok(mid) = skull::user_module_id(&ws)
    {
        let _ = skull::mir_emit::emit_full(&ws, mid);
    }
}

/// Even seeds mutate a generated program, odd seeds a corpus file.
fn source(seed: u64) -> (String, String) {
    if seed.is_multiple_of(2) {
        return (format!("generated program {seed}"), program(seed));
    }
    let mut files: Vec<_> = walkdir::WalkDir::new(crate::support::repo_root().join("tests"))
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.into_path())
        .filter(|path| {
            path.extension().is_some_and(|e| e == "rock")
                && !path.components().any(|c| c.as_os_str() == "errors")
        })
        .collect();
    files.sort();
    let path = &files[(seed / 2) as usize % files.len()];
    let text = std::fs::read_to_string(path).expect("corpus file");
    (path.display().to_string(), text)
}

pub(super) fn check(seed: u64, count: usize) -> Result<(), String> {
    let (origin, source) = source(seed);
    let spans = tokens(&source);
    let mut random = seed ^ 0x6d75_7461_7465;
    let mut pick = |bound: usize| {
        random = random
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((random >> 33) % bound as u64) as usize
    };
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let file = dir.path().join("main.rock");
    std::fs::write(&file, &source).map_err(|e| e.to_string())?;
    let (main, incremental) = (
        dir.path().join("main.lua"),
        dir.path().join("incremental.lua"),
    );
    let root = dir.path().to_str().expect("utf-8 tempdir");
    // A corpus file that needs its siblings has no live baseline.
    let mut live = IncrementalProject::init(root, &main, &incremental).ok();
    let path = file.to_str().expect("utf-8 path");
    let mut editor = BoneWorkspace::new();
    // Live and editor state carry over from the previous mutation.
    let mut previous = source.clone();
    for step in 0..count {
        let (mutated, offset, what) = mutate(&source, &spans, &mut pick);
        let at = position(&mutated, offset.min(mutated.len()));
        let lines = mutated.lines().count() as u32 + 1;
        let stages: [(&str, &mut dyn FnMut()); 3] = [
            ("check and compile", &mut || fresh(&mutated)),
            ("live edit", &mut || {
                if let Some(live) = &mut live {
                    let _ = live.apply_edit(&file, &mutated, &main, &incremental);
                }
            }),
            ("editor queries", &mut || {
                editor.update(path, mutated.clone());
                editor.completion(path, at);
                editor.hover(path, at);
                editor.definition(path, at);
                editor.references(path, at);
                editor.signature_help(path, at);
                editor.document_highlight(path, at);
                let _ = editor.prepare_rename(path, at);
                editor.document_symbols(path);
                editor.semantic_tokens(path);
                editor.inlay_hints(
                    path,
                    Range::new(Position::new(0, 0), Position::new(lines, 0)),
                );
            }),
        ];
        for (stage, run) in stages {
            if let Err(payload) = catch_unwind(AssertUnwindSafe(run)) {
                if let Some(output) = std::env::var_os("ROCK_DIFF_OUTPUT") {
                    let output = std::path::Path::new(&output);
                    std::fs::create_dir_all(output).unwrap();
                    std::fs::write(
                        output.join(format!("mutation-{seed}-before.rock")),
                        &previous,
                    )
                    .unwrap();
                    std::fs::write(output.join(format!("mutation-{seed}-after.rock")), &mutated)
                        .unwrap();
                }
                return Err(format!(
                    "{origin}, step {step}: {what} at {}:{}: {stage} panicked: {}\nmutated source:\n{mutated}",
                    at.line + 1,
                    at.character + 1,
                    panic_text(payload.as_ref())
                ));
            }
        }
        previous = mutated;
    }
    Ok(())
}
