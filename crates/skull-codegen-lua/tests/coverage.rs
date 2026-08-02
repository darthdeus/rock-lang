//! Corpus lowering+emit coverage probe — the Phase-6 parity lens.
//!
//! Walks the whole `tests/` corpus, parses each file alongside the real stdlib,
//! runs bone's `semantic_pass`, then for every user-module function attempts the
//! NEW path (`skull_mir::lower::lower_function` -> `emit_function`) and tallies
//! success vs gap-reason. The gap histogram is what drives the cutover: it names
//! exactly which constructs the new path must learn before it can replace old
//! skull. Ignored by default (it's a measurement, not a gate); run with:
//!   cargo test -p skull-codegen-lua --test coverage -- --ignored --nocapture

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use skull_codegen_lua::{emit_function, CodegenCtx};
use skull_mir::mir::FuncRef;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = ustr::Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn collect_rock_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_rock_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rock")
            && !p.to_string_lossy().ends_with(".rock.err")
        {
            out.push(p);
        }
    }
}

/// Reduce a gap message to its head ("skull-mir: lowering gap: <head> ...") so
/// distinct details collapse into one histogram bucket.
fn gap_bucket(msg: &str) -> String {
    let m = msg.strip_prefix("skull-mir: lowering gap: ").unwrap_or(msg);
    // Keep the first few words; drop trailing specifics in parens/quotes.
    let head: String = m.chars().take_while(|&c| c != '(' && c != '"').collect();
    head.trim().split_whitespace().take(8).collect::<Vec<_>>().join(" ")
}

#[test]
#[ignore = "measurement probe; run with --ignored --nocapture"]
fn corpus_lowering_coverage() {
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
    let mut files = Vec::new();
    collect_rock_files(&tests_dir, &mut files);
    files.sort();

    // Silence bone inference panics on bad inputs; count them per file.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let mut total_fns = 0usize;
    let mut ok_fns = 0usize;
    let mut gaps: BTreeMap<String, usize> = BTreeMap::new();
    let mut files_ok = 0usize;
    let mut files_sema_fail = 0usize;
    let mut files_panic = 0usize;

    for file in &files {
        let Ok(src) = std::fs::read_to_string(file) else { continue };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let Ok(user) = skull_parser::parse_module_clean(&src, ustr::Ustr::from("<test>")) else {
                return None;
            };
            let mut modules = STDLIB.clone();
            modules.push(user);
            let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).ok()?;
            let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>"))?;
            let ctx = CodegenCtx::new(&ws);
            let scx = skull_mir::lower::ScalarCtx::new(&ws);
            let mut local_total = 0usize;
            let mut local_ok = 0usize;
            let mut local_gaps: Vec<String> = Vec::new();
            for (fid, _) in ws.module(mid).functions.iter() {
                local_total += 1;
                match skull_mir::lower::lower_function(&ws, &scx, mid, fid) {
                    Ok(func) => {
                        // Emit too — a lower-OK that panics at emit is not parity.
                        let _ = emit_function(&ws, &ctx, FuncRef { module: mid, func: fid }, &func);
                        local_ok += 1;
                    }
                    Err(e) => local_gaps.push(gap_bucket(&e.message)),
                }
            }
            Some((local_total, local_ok, local_gaps))
        }));
        match outcome {
            Ok(Some((t, o, g))) => {
                total_fns += t;
                ok_fns += o;
                for b in g {
                    *gaps.entry(b).or_insert(0) += 1;
                }
                if t == o {
                    files_ok += 1;
                }
            }
            Ok(None) => files_sema_fail += 1,
            Err(_) => files_panic += 1,
        }
    }
    std::panic::set_hook(prev);

    let pct = if total_fns > 0 { 100.0 * ok_fns as f64 / total_fns as f64 } else { 0.0 };
    println!("\n=== skull new-path lowering coverage over tests/ ===");
    println!("files: {} total, {} fully-lowered, {} sema-fail (multi-file/.err), {} panic",
        files.len(), files_ok, files_sema_fail, files_panic);
    println!("functions: {ok_fns}/{total_fns} lowered+emitted ({pct:.1}%)");
    println!("\ngap histogram (function count by reason):");
    let mut sorted: Vec<_> = gaps.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));
    for (reason, n) in sorted {
        println!("  {n:>5}  {reason}");
    }
}
