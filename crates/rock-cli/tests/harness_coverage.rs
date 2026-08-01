//! `autotests = false` means a new `tests/*.rs` is not a test target unless it
//! is declared in that crate's harness root -- it would compile, run nothing,
//! and read as green. This pins every merged crate's harness to its directory.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// (crate dir, harness roots that together must declare every test file)
const MERGED: &[(&str, &[&str])] = &[
    ("skull", &["tests/main.rs"]),
    ("skull-codegen-js", &["tests/main.rs"]),
    ("bone-query", &["tests/main.rs"]),
    ("rock-cli", &["tests/main.rs", "tests/main_heavy.rs"]),
];

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/<name> has a parent")
        .to_path_buf()
}

#[test]
fn every_test_file_is_declared_in_a_harness() {
    let mut undeclared = Vec::new();
    for (krate, roots) in MERGED {
        let tests = crates_dir().join(krate).join("tests");
        let declared: HashSet<String> = roots
            .iter()
            .flat_map(|r| {
                let src = std::fs::read_to_string(crates_dir().join(krate).join(r))
                    .unwrap_or_else(|e| panic!("{krate}/{r}: {e}"));
                src.lines()
                    .filter_map(|l| {
                        let l = l.trim();
                        let rest = l.strip_prefix("#[path = \"")?;
                        Some(rest.strip_suffix("\"]")?.to_string())
                    })
                    .collect::<Vec<_>>()
            })
            .collect();

        for entry in std::fs::read_dir(&tests).expect("tests dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            // Harness roots declare the others; they are not declared themselves.
            if roots.iter().any(|r| r.ends_with(&name)) {
                continue;
            }
            if !declared.contains(&name) {
                undeclared.push(format!("{krate}/tests/{name}"));
            }
        }
    }
    undeclared.sort();
    assert!(
        undeclared.is_empty(),
        "test files not declared in any harness root (they would never run):\n  {}",
        undeclared.join("\n  ")
    );
}
