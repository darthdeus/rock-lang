//! A libtest-style runner for the fixture files: one test per file, run in
//! parallel, filtered by the usual `cargo test` arguments. Kept dependency-free
//! because a clap-based harness changes the features the whole workspace is
//! built with under `cargo test`, which recompiles most of it.

use bone_core::prelude::Instant;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, mpsc};

struct Args {
    filters: Vec<String>,
    skip: Vec<String>,
    exact: bool,
    list: bool,
    ignored_only: bool,
    quiet: bool,
    threads: Option<usize>,
}

fn parse_args() -> std::result::Result<Args, String> {
    let mut args = Args {
        filters: Vec::new(),
        skip: Vec::new(),
        exact: false,
        list: false,
        ignored_only: false,
        quiet: false,
        threads: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if arg.starts_with("--") => (flag.to_string(), Some(value.into())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| {
            inline
                .clone()
                .or_else(|| it.next())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match flag.as_str() {
            "--exact" => args.exact = true,
            "--list" => args.list = true,
            "--ignored" => args.ignored_only = true,
            "-q" | "--quiet" => args.quiet = true,
            "--include-ignored" | "--nocapture" | "--show-output" => {}
            "--skip" => args.skip.push(value("--skip")?),
            "--test-threads" => {
                let n = value("--test-threads")?;
                args.threads = Some(
                    n.parse()
                        .ok()
                        .filter(|&n| n > 0)
                        .ok_or_else(|| format!("invalid --test-threads value `{n}`"))?,
                );
            }
            // Accepted for compatibility with tools that pass libtest options.
            "--format" | "--color" | "--logfile" | "--shuffle-seed" | "-Z" => {
                value(&flag)?;
            }
            _ if arg.starts_with('-') => return Err(format!("unknown option `{arg}`")),
            _ => args.filters.push(arg),
        }
    }
    Ok(args)
}

fn collect(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read fixture directory {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("fixture directory entry");
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if path.is_dir() {
            collect(&path, &rel, out);
        } else if name.ends_with(".rock") {
            out.push((rel, path));
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    match payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
    {
        Some(msg) => format!("test panicked: {msg}"),
        None => "test panicked".to_string(),
    }
}

/// Runs `check(file_name, contents)` for every `.rock` file under `root`
/// (relative to the crate), naming each test `<prefix>::<relative path>`.
pub fn run(
    prefix: &str,
    root: &str,
    check: fn(&str, &str) -> std::result::Result<(), String>,
) -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(101);
        }
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(root);
    let mut all = Vec::new();
    collect(&root, "", &mut all);
    all.sort();
    let total = all.len();

    let selected = |name: &str| {
        let matches = |pattern: &String| {
            if args.exact {
                name == pattern
            } else {
                name.contains(pattern.as_str())
            }
        };
        (args.filters.is_empty() || args.filters.iter().any(matches))
            && !args.skip.iter().any(matches)
    };
    let tests: Vec<(String, PathBuf)> = all
        .into_iter()
        .map(|(rel, path)| (format!("{prefix}::{rel}"), path))
        .filter(|(name, _)| selected(name))
        .filter(|_| !args.ignored_only)
        .collect();

    if args.list {
        for (name, _) in &tests {
            println!("{name}: test");
        }
        return ExitCode::SUCCESS;
    }

    let filtered_out = total - tests.len();
    let count = tests.len();
    println!(
        "\nrunning {count} test{}",
        if count == 1 { "" } else { "s" }
    );
    let start = Instant::now();
    let threads = args
        .threads
        .or_else(|| std::thread::available_parallelism().ok().map(Into::into))
        .unwrap_or(1)
        .min(count.max(1));

    let queue = Mutex::new(tests.into_iter());
    let (tx, rx) = mpsc::channel();
    let mut failures = Vec::new();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let tx = tx.clone();
            let queue = &queue;
            scope.spawn(move || {
                loop {
                    let Some((name, path)) = queue.lock().unwrap().next() else {
                        break;
                    };
                    let file_name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let outcome = match std::fs::read_to_string(&path) {
                        Err(e) => Err(format!("error reading file '{}': {e}", path.display())),
                        Ok(contents) => {
                            catch_unwind(AssertUnwindSafe(|| check(&file_name, &contents)))
                                .unwrap_or_else(|payload| Err(panic_message(&*payload)))
                        }
                    };
                    let _ = tx.send((name, outcome));
                }
            });
        }
        drop(tx);
        for (name, outcome) in rx {
            let ok = outcome.is_ok();
            if !args.quiet {
                println!("test {name} ... {}", if ok { "ok" } else { "FAILED" });
            } else if !ok {
                println!("test {name} ... FAILED");
            }
            if let Err(msg) = outcome {
                failures.push((name, msg));
            }
        }
    });

    failures.sort();
    if !failures.is_empty() {
        println!("\nfailures:\n");
        for (name, msg) in &failures {
            println!("---- {name} ----\n{msg}\n");
        }
        println!("failures:");
        for (name, _) in &failures {
            println!("    {name}");
        }
    }
    let failed = failures.len();
    println!(
        "\ntest result: {}. {} passed; {failed} failed; 0 ignored; 0 measured; \
         {filtered_out} filtered out; finished in {:.2}s\n",
        if failed == 0 { "ok" } else { "FAILED" },
        count - failed,
        start.elapsed().as_secs_f64()
    );
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(101)
    }
}
