//! Seeded typed-program generation and differential reduction; see docs/testing.md.
#![cfg(feature = "backend-js")]

mod generator;
mod runner;

use crate::support::require_node;
use generator::program;

fn compare(seed: u64) {
    let source = program(seed);
    if let Some(output) = std::env::var_os("ROCK_DIFF_OUTPUT") {
        std::fs::create_dir_all(&output).unwrap();
        std::fs::write(
            std::path::Path::new(&output).join(format!("seed-{seed}.rock")),
            &source,
        )
        .unwrap();
    }
    if let Err(finding) = runner::check(&source) {
        eprintln!("seed {seed}: {finding}\noriginal source:\n{source}");
        let minimized = runner::minimize(&source, &finding);
        if let Some(output) = std::env::var_os("ROCK_DIFF_OUTPUT") {
            std::fs::write(
                std::path::Path::new(&output).join(format!("seed-{seed}-min.rock")),
                &minimized,
            )
            .unwrap();
        }
        panic!("seed {seed}: {finding}\nminimized source:\n{minimized}");
    }
}

#[test]
fn smoke() {
    if require_node() {
        compare(2);
    }
}

#[test]
#[ignore = "long differential campaign; set ROCK_DIFF_SEEDS and ROCK_DIFF_START"]
fn campaign() {
    assert!(require_node(), "a campaign requires node");
    let setting =
        |name, default| std::env::var(name).map_or(default, |s| s.parse::<u64>().unwrap());
    let start = setting("ROCK_DIFF_START", 0);
    let count = setting("ROCK_DIFF_SEEDS", 100);
    assert!(count > 0);
    for offset in 0..count {
        let seed = start.wrapping_add(offset);
        compare(seed);
        eprintln!("differential seed {seed} passed");
    }
}
