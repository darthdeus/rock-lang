//! End-to-end execution oracle for the new (cell-model) emit path.
//!
//! For each corpus file: parse with the real stdlib, run bone semantic_pass,
//! assemble a full Lua program via `skull::mir_emit::emit_workspace_mir`, append
//! an in-process test runner, and run it ONCE under luajit. The runner overrides
//! the global `print` (rock's `print` is `extern(lua)` -> the Lua global) to
//! capture each `@test`'s stdout, then compares a `@test(output = "...")` to its
//! declared output (a plain `@test` must merely run clean). This is what proves
//! the new path is *correct*, not just that it emits
//! (`docs/skull-bone-mir-faithful-plan.md` Phase 6).
//!
//! Ignored by default; run with:
//!   cargo test -p skull --test mir_execution_oracle -- --ignored --nocapture

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = ustr::Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
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

/// Run `luajit <prog>` with a wall-clock timeout (seconds); `None` on timeout.
fn run_with_timeout(prog: &Path, secs: u64) -> Option<std::process::Output> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("luajit")
        .arg(prog)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        if child.try_wait().ok()?.is_some() {
            return child.wait_with_output().ok();
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn lua_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            _ => o.push(c),
        }
    }
    o.push('"');
    o
}

/// The Lua test-runner footer: override `print` to capture, run each test, emit
/// a `@@R@@<TAB>name<TAB>verdict` line for the harness to parse.
fn runner_footer(tests: &[skull::SkullTest]) -> String {
    // Capture BOTH `print` (newline-terminated) and `io.write` (raw) into a
    // single string accumulator — some tests build output via `io.write`
    // (`put(...)`) with explicit `\n`s, so a print-only capture saw "".
    let mut f = String::from(
        "\nlocal __op = print\nlocal __buf = \"\"\n\
         print = function(...)\n  local n = select('#', ...)\n  local p = {}\n  for i=1,n do p[i]=tostring(select(i,...)) end\n  __buf = __buf .. table.concat(p,\"\\t\") .. \"\\n\"\nend\n\
         io.write = function(...)\n  local n = select('#', ...)\n  for i=1,n do __buf = __buf .. tostring(select(i,...)) end\nend\n\
         local function __run(name, expect)\n  local fn = _G[name]\n  if fn == nil then __op(\"@@R@@\",name,\"NODEF\"); return end\n  __buf=\"\"\n  local ok,err=pcall(fn)\n  if not ok then __op(\"@@R@@\",name,\"ERR\",tostring(err)); return end\n  if expect==nil then __op(\"@@R@@\",name,\"OKNOEXP\"); return end\n  local g=__buf:gsub(\"%s+$\",\"\")\n  local e=expect:gsub(\"%s+$\",\"\")\n  __op(\"@@R@@\",name,(g==e) and \"MATCH\" or \"MISMATCH\", g)\nend\n",
    );
    for t in tests {
        let exp = match &t.output {
            Some(s) => lua_str(s),
            None => "nil".to_string(),
        };
        f.push_str(&format!("__run({}, {})\n", lua_str(&t.name), exp));
    }
    f
}

/// `(file, test)` pairs where skull's output INTENTIONALLY differs from the
/// `@test(output=)` annotation because the annotation encodes a rock bug skull
/// does not replicate. Reported as KNOWN (non-fatal), not MISMATCH — so the
/// mismatch count tracks real skull problems. Each must be justified:
///
/// - `licm_global_write_across_call_sentinel`: the documented LICM sentinel.
/// - `strings_comprehensive::integer_width_basic`: rock's FIRST `string.format`
///   width call in a process fails to pad (`"{x:5}"` -> "42" not "   42"; a
///   warmup format call fixes it — verified against the live `rock` process).
///   skull emits correct printf; the annotation encodes rock's first-call bug.
fn is_known_divergence(file: &str, test: &str) -> bool {
    matches!(
        (file, test),
        ("licm_global_write_across_call_sentinel.rock", _)
            | ("strings_comprehensive.rock", "integer_width_basic")
    )
}

#[test]
#[ignore = "execution oracle; run with --ignored --nocapture"]
fn corpus_execution_oracle() {
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests");
    let mut files = Vec::new();
    collect_rock_files(&tests_dir, &mut files);
    files.sort();

    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let (mut files_total, mut files_clean, mut files_sema, mut files_panic, mut files_load_err) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    let (mut pass, mut mismatch, mut err, mut nodef) = (0usize, 0usize, 0usize, 0usize);
    let mut known = 0usize;
    let mut mismatch_examples: Vec<String> = Vec::new();
    let mut load_err_buckets: BTreeMap<String, usize> = BTreeMap::new();
    let seq = std::sync::atomic::AtomicU64::new(0);

    for file in &files {
        files_total += 1;
        let Ok(src) = std::fs::read_to_string(file) else { continue };
        // Parse with the real file name as the module id so `__SITE__`
        // (bone lowers it to "<file>:<line>") matches the tests' filename
        // assertions, rather than a placeholder "<test>".
        let modname = file.file_name().unwrap().to_string_lossy().to_string();
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let user = skull_parser::parse_module(&src, ustr::Ustr::from(modname.as_str())).ok()?;
            let mut modules = STDLIB.clone();
            modules.push(user);
            let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).ok()?;
            let mid = *ws.module_ids.get(&ustr::Ustr::from(modname.as_str()))?;
            let emit = skull::mir_emit::emit_workspace_mir(&ws, mid).ok()?;
            let tests = skull::module_tests(&ws, mid);
            Some((emit.lua, emit.skipped.len(), tests))
        }));
        let (mut lua, skipped, tests) = match built {
            Ok(Some(x)) => x,
            Ok(None) => {
                files_sema += 1;
                continue;
            }
            Err(_) => {
                files_panic += 1;
                continue;
            }
        };
        if tests.is_empty() {
            continue;
        }
        lua.push_str(&runner_footer(&tests));
        let n = seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let prog = std::env::temp_dir().join(format!("skull_oracle_{}_{}.lua", std::process::id(), n));
        if std::fs::write(&prog, &lua).is_err() {
            continue;
        }
        let Some(out) = run_with_timeout(&prog, 5) else {
            // A miscompiled test can infinite-loop; a timeout is a run-error for
            // every test in the file (we can't attribute it to one).
            let _ = std::fs::remove_file(&prog);
            err += tests.len();
            files_load_err += 1;
            *load_err_buckets.entry("TIMEOUT (likely infinite loop)".into()).or_insert(0) += 1;
            continue;
        };
        let _ = std::fs::remove_file(&prog);
        // A program-level load/parse error (no @@R@@ lines) -> the whole file is
        // a load error (e.g. a construct we mis-emitted).
        let stdout = String::from_utf8_lossy(&out.stdout);
        let result_lines: Vec<&str> = stdout.lines().filter(|l| l.starts_with("@@R@@")).collect();
        if result_lines.is_empty() {
            files_load_err += 1;
            let stderr = String::from_utf8_lossy(&out.stderr);
            let raw = stderr.lines().next().unwrap_or("(no stderr)");
            let msg = raw.rsplitn(2, ".lua:").next().unwrap_or(raw);
            *load_err_buckets.entry(format!("{} [{}]", msg, file.file_name().unwrap().to_string_lossy())).or_insert(0) += 1;
            continue;
        }
        let mut file_clean = skipped == 0;
        for line in result_lines {
            let parts: Vec<&str> = line.splitn(4, '\t').collect();
            let (name, verdict) = (parts.get(1).copied().unwrap_or(""), parts.get(2).copied().unwrap_or(""));
            match verdict {
                "MATCH" | "OKNOEXP" => pass += 1,
                "MISMATCH" if is_known_divergence(
                    &file.file_name().unwrap().to_string_lossy(),
                    name,
                ) =>
                {
                    // skull is correct; the annotation encodes a rock bug.
                    known += 1;
                }
                "MISMATCH" => {
                    mismatch += 1;
                    file_clean = false;
                    if mismatch_examples.len() < 60 {
                        mismatch_examples.push(format!(
                            "MISMATCH {}::{}  got {:?}",
                            file.file_name().unwrap().to_string_lossy(),
                            name,
                            parts.get(3).copied().unwrap_or("")
                        ));
                    }
                }
                "ERR" => {
                    err += 1;
                    file_clean = false;
                    // Bucket the lua error: "call global 'X' (nil)" = a gapped
                    // callee (expected); anything else may be a real emit bug.
                    let msg = parts.get(3).copied().unwrap_or("");
                    let is_gapped_callee = msg.contains("attempt to call global '");
                    let bucket = if is_gapped_callee {
                        "call-global-nil (gapped callee)".to_string()
                    } else {
                        msg.rsplit(".lua:").next().unwrap_or(msg)
                            .trim_start_matches(|c: char| c.is_ascii_digit() || c == ':' || c == ' ')
                            .chars().take(50).collect::<String>()
                    };
                    // A non-gapped-callee ERR may be a real emit bug — list it.
                    if !is_gapped_callee && mismatch_examples.len() < 60 {
                        mismatch_examples.push(format!(
                            "ERR {}::{}  {}",
                            file.file_name().unwrap().to_string_lossy(),
                            name,
                            msg.chars().take(70).collect::<String>()
                        ));
                    }
                    *load_err_buckets.entry(format!("ERR: {bucket}")).or_insert(0) += 1;
                }
                "NODEF" => {
                    nodef += 1;
                    file_clean = false;
                }
                _ => {}
            }
        }
        if file_clean {
            files_clean += 1;
        }
    }
    std::panic::set_hook(prev);

    let total = pass + known + mismatch + err + nodef;
    println!("\n=== skull new-path EXECUTION oracle over tests/ ===");
    println!(
        "files: {files_total} total | {files_clean} clean (no skips, all tests pass) | {files_sema} sema/emit-fail | {files_load_err} program-load-error | {files_panic} panic"
    );
    println!(
        "tests: {pass}/{total} pass | {known} known-divergence | {mismatch} output-mismatch | {err} run-error | {nodef} no-def (gapped fn)"
    );
    if !mismatch_examples.is_empty() {
        println!("\nfirst {} mismatch/error examples:", mismatch_examples.len());
        for m in &mismatch_examples {
            println!("  {m}");
        }
    }
    if !load_err_buckets.is_empty() {
        println!("\nprogram-load-error buckets:");
        let mut v: Vec<_> = load_err_buckets.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        for (k, n) in v.iter().take(20) {
            println!("  {n:>4}  {k}");
        }
    }
}
