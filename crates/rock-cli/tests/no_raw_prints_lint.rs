//! Phase 5 of the logging unification (docs/logging-architecture.md):
//! the regression lock. After migrating ~57 stone-rs + ~130 rock-cli
//! call sites to the one logger, nothing *enforced* "no bare
//! `eprintln!`/`println!` outside the sanctioned places" — so new
//! code would silently bypass the unified logger and the whole
//! migration would rot. This test is that enforcement; it runs in the
//! standard `cargo nextest` gate.
//!
//! Rule: every raw print macro in `stone-rs/src` and `rock-cli/src`
//! must be either (a) already migrated away, or (b) a documented
//! carve-out (result output / harness output / ariadne error render —
//! these are the command's *function*, not logging, and stay on
//! stdout, pipeable). The carve-outs are grandfathered as a per-file
//! **count baseline** (one comment per multi-line result block, not
//! per line, so a line-precise allowlist is impractical). The test
//! fails when a file's raw-print count *changes* vs the baseline:
//!
//!  - count went UP  -> a new raw print slipped in. Route it through
//!    `log::{info,warn,error,debug,trace}!(target: "CAT", …)` (Rust)
//!    / `log_*` (Rock). If it is genuinely result output (stdout data
//!    the command exists to produce), add a
//!    `// Result output, NOT logging: …` comment AND bump this file's
//!    baseline in the same change.
//!  - count went DOWN -> you migrated/removed one (good). Lower the
//!    baseline to the new count so the ratchet keeps tightening.
//!
//! `rock-log/src/lib.rs` (the logger backend, the one sanctioned
//! `eprintln!`) is out of scope here — it is not under the scanned
//! roots.

use std::path::Path;

/// Accepted raw-print counts per file (carve-outs: result output /
/// test+profiling harness output / ariadne error render). Anything
/// not listed must have ZERO raw prints. Keep this list shrinking.
const BASELINE: &[(&str, usize)] = &[
    ("rock-cli/src/ab.rs", 8),
    ("rock-cli/src/comment_fuzz.rs", 11),
    ("rock-cli/src/compile_cmd.rs", 25),
    ("rock-cli/src/dispatcher.rs", 2),
    ("rock-cli/src/error_test.rs", 11),
    ("rock-cli/src/fmt_test.rs", 10),
    ("rock-cli/src/formatting.rs", 12),
    ("rock-cli/src/incremental_profile.rs", 41),
    ("rock-cli/src/lsp_test.rs", 70),
    ("rock-cli/src/main.rs", 1),
    ("rock-cli/src/parallel_runner.rs", 6),
    ("rock-cli/src/run_sim.rs", 7),
    ("rock-cli/src/stick.rs", 33),
    ("rock-cli/src/stone.rs", 8),
    ("rock-cli/src/test.rs", 35),
    ("rock-cli/src/unified_compile.rs", 1),
];

const MACROS: &[&str] = &["eprintln!", "println!", "eprint!", "print!"];

/// Count raw print-macro *invocations* in `src`. Mirrors the
/// baseline-generation scan exactly: skip `//`-comment lines (incl.
/// `///`/`//!`); a hit is `<macro>` whose preceding char is not
/// `[A-Za-z_]` (so `my_print!` / `sprint!` don't match) followed by
/// optional whitespace then `(` (the call). Counting `eprintln!`
/// before `println!`, and requiring the `!(`, disambiguates the
/// overlapping names without double counting.
fn count_raw_prints(src: &str) -> usize {
    let mut n = 0;
    for line in src.lines() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let b = line.as_bytes();
        for mac in MACROS {
            let m = mac.as_bytes();
            let mut i = 0;
            while let Some(rel) = line[i..].find(mac) {
                let start = i + rel;
                let prev_ok = start == 0 || {
                    let p = b[start - 1];
                    !(p.is_ascii_alphabetic() || p == b'_')
                };
                // after the macro name: optional ws then '('
                let mut j = start + m.len();
                while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                    j += 1;
                }
                if prev_ok && j < b.len() && b[j] == b'(' {
                    n += 1;
                }
                i = start + m.len();
            }
        }
    }
    n
}

fn collect_rs(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[ignore = "this is pointless, but beforew we nuke it we want to nuke the things referencing it and simplify the rest"]
#[test]
fn no_new_raw_prints_outside_carveouts() {
    // CARGO_MANIFEST_DIR = <ws>/rock-cli; workspace root is its parent.
    let ws = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf();

    let baseline: std::collections::HashMap<&str, usize> =
        BASELINE.iter().copied().collect();

    let mut files = Vec::new();
    collect_rs(&ws.join("stone-rs/src"), &mut files);
    collect_rs(&ws.join("rock-cli/src"), &mut files);
    files.sort();

    let mut violations = Vec::new();
    for f in &files {
        let rel = f
            .strip_prefix(&ws)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let src = std::fs::read_to_string(f).unwrap_or_default();
        let n = count_raw_prints(&src);
        let allowed = baseline.get(rel.as_str()).copied().unwrap_or(0);
        if n != allowed {
            violations.push(format!(
                "  {rel}: {n} raw print(s), baseline {allowed}"
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "Raw-print baseline mismatch (Phase 5 logging lint, \
         docs/logging-architecture.md):\n{}\n\n\
         UP   => a new raw print slipped in. Route it through the \
         unified logger: log::{{info,warn,error,debug,trace}}!(target: \
         \"CAT\", ..) in Rust, log_* in Rock. If it is genuinely \
         result output (stdout data the command exists to produce, \
         pipeable) add a `// Result output, NOT logging: ..` comment \
         AND bump this file's BASELINE entry in the same change.\n\
         DOWN => you migrated/removed one (good) — lower this file's \
         BASELINE to the new count so the ratchet keeps tightening.",
        violations.join("\n")
    );
}
