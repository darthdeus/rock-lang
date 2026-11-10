//! **The graph walk does not depend on how many threads run it.**
//!
//! Owners of a wave are walked on the pool while one thread installs the
//! finished walks in wave order. On a one-thread pool every walk runs inline,
//! in install order, which is the plain wave-by-wave walk; every other pool
//! size must build the same graph and visit the same owners.

use crate::common::{stdlib_asts, try_ws_of_files};
use std::path::{Path, PathBuf};

use bone_incremental::session::{NsAliases, Session, SessionFile};
use bone_semantic::inference::Workspace;
use skull::graph_snapshot::canonical;
use skull::instance_graph::{self, InstanceGraph, RunStats};

const PROJ: &str = "proj/";

fn pool(threads: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .expect("thread pool")
}

fn build_on(pool: &rayon::ThreadPool, ws: &Workspace) -> InstanceGraph {
    pool.install(|| {
        instance_graph::build(ws, skull::mir_emit::emission_roots(ws, Some(PROJ)), false)
    })
}

/// The visit census: which owners were walked, and why.
fn visits(stats: &RunStats) -> [usize; 7] {
    [
        stats.owner_visits,
        stats.expanded,
        stats.first_owner_visits,
        stats.duplicate_pending_visits,
        stats.witness_settlement_rewalks,
        stats.incremental_forced_rewalks,
        stats.settle_rounds,
    ]
}

#[track_caller]
fn assert_same(label: &str, ws: &Workspace, serial: &InstanceGraph, pooled: &InstanceGraph) {
    let diff = canonical(ws, serial).diff(&canonical(ws, pooled));
    assert!(
        diff.is_empty(),
        "{label}: the pooled walk built a different graph:\n{}",
        diff.join("\n")
    );
    assert_eq!(
        visits(&serial.run_stats),
        visits(&pooled.run_stats),
        "{label}: the pooled walk visited different owners"
    );
}

fn corpus_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("corpus directory")
        .map(|e| e.expect("corpus entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            corpus_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rock") {
            out.push(path);
        }
    }
}

#[test]
fn every_corpus_program_walks_the_same_on_any_pool() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
    let mut files = Vec::new();
    corpus_files(&root, &mut files);
    let (serial, pooled) = (pool(1), pool(8));
    let mut programs = 0;
    let mut owners = 0;
    for path in &files {
        let src = std::fs::read_to_string(path).expect("corpus source");
        let Some(ws) = try_ws_of_files(&[("proj/main.rock", &src)]) else {
            continue;
        };
        let a = build_on(&serial, &ws);
        let b = build_on(&pooled, &ws);
        let label = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .display()
            .to_string();
        assert_same(&label, &ws, &a, &b);
        programs += 1;
        owners += a.run_stats.expanded;
    }
    assert!(
        programs >= 300 && owners >= 100_000,
        "the corpus sweep compared only {programs} programs and {owners} owner walks"
    );
}

const MAIN_V1: &str = "\
fn zz_start() -> int {
    let v: Vec<int> = vec![1, 2, 3]
    let t = 0
    for x in v.iter().map(_ * 2) { t = t + x }
    return t + pick(v, 1) + twice(3)
}
";

const MAIN_V2: &str = "\
fn zz_start() -> int {
    let v: Vec<int> = vec![1, 2, 3]
    let w: Vec<string> = vec![\"a\"]
    let t = 0
    for x in v.iter().filter(_ > 1) { t = t + x }
    return t + pick(v, 1) + pick(w, 0).len()
}
";

const LIB: &str = "\
fn pick<T>(v: Vec<T>, i: int) -> T { return v[i] }
fn twice(n: int) -> int { return n * 2 }
";

/// The update path re-walks through the same loop, so it is held to the same
/// property: one edit applied to a serially built graph and to a pooled one.
#[test]
fn an_incremental_update_walks_the_same_on_any_pool() {
    let mut session = Session::new(
        stdlib_asts(),
        vec![
            SessionFile::legacy("proj/main.rock", MAIN_V1),
            SessionFile::legacy("lib/helpers.rock", LIB),
        ],
        &NsAliases::default(),
    )
    .expect("session");
    let (serial, pooled) = (pool(1), pool(8));
    let mut a = build_on(&serial, session.workspace());
    let mut b = build_on(&pooled, session.workspace());
    assert_same("before the edit", session.workspace(), &a, &b);

    let res = session
        .update_file("proj/main.rock", MAIN_V2)
        .expect("incremental update");
    assert!(!res.reinferred.is_empty(), "the edit reinferred nothing");
    let ws = session.workspace();
    let roots = skull::mir_emit::emission_roots(ws, Some(PROJ));
    let edits = res.body_update.edit.into_iter().collect::<Vec<_>>();
    let sa =
        serial.install(|| a.update_bodies_with_edits(ws, roots.clone(), &res.reinferred, &edits));
    let sb = pooled.install(|| b.update_bodies_with_edits(ws, roots, &res.reinferred, &edits));
    assert!(sa.expanded > 0, "the update walked no owners");
    assert_eq!(
        (sa.affected, sa.expanded, sa.pruned),
        (sb.affected, sb.expanded, sb.pruned)
    );
    assert_same("after the edit", ws, &a, &b);
}
