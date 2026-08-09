//! **The persistent `InstanceGraph` updater against real projects**
//! (`docs/parked/mono.md`). The compact fixtures in
//! `crates/skull/tests/graph_update.rs` cover one causal producer each; this is
//! the breadth check, and the only place the updater meets a program it did not
//! have written for it.
//!
//! Two things are asserted, and the second is what makes the first worth
//! anything:
//!
//! - the updated graph is canonically EQUAL to a fresh build of the same
//!   workspace, after a deletion and after an addition;
//! - the update SCALED with the edit. Equality alone cannot tell an update from
//!   a hidden rebuild -- a rebuild reaches the right answer too -- so every arm
//!   states the walked population against the total in one assertion.
//!
//! Heavy by construction: it reads three projects off disk and warms the real
//! stdlib, so it lives in the `-P full` harness.

use std::collections::BTreeSet;
use std::time::Instant;

use bone_incremental::session::{Session, UpdateTier};
use bone_semantic::inference::Workspace;
use skull::graph_snapshot::canonical;
use skull::instance_graph::{self, InstanceGraph, UpdateStats};

fn warm_stdlib() -> Vec<skull_parser::Ast> {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = ustr::Ustr::from(format!("<bone-stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id)
                .unwrap_or_else(|e| panic!("stdlib {} must parse: {e:?}", f.name))
        })
        .collect()
}

fn project_dir(rel: &str) -> String {
    format!("{}/../../{rel}", env!("CARGO_MANIFEST_DIR"))
}

fn project_files(path: &str) -> Vec<(String, String)> {
    let spec = rock_cli::common::path_to_source_spec(path);
    let sources = rock_cli::unified_compile::discover_sources(&spec).expect("discovery");
    sources
        .files
        .iter()
        .map(|f| {
            let f = f.borrow();
            (f.file.to_id_path(), f.contents.clone())
        })
        .collect()
}

fn graph_of(ws: &Workspace, root: &str) -> InstanceGraph {
    instance_graph::build(ws, instance_graph::emission_roots(ws, Some(root)), false)
}

/// A bare call STATEMENT: a line that is exactly `name(...)`, with no `let`, no
/// `return` and no assignment, so commenting it out removes a call without
/// leaving a binding unbound.
fn is_bare_call_statement(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || t.starts_with("//") || t.contains('=') {
        return false;
    }
    for kw in ["let ", "return ", "if ", "for ", "while ", "match ", "yield ", "defer "] {
        if t.starts_with(kw) {
            return false;
        }
    }
    let Some(open) = t.find('(') else { return false };
    if !t.ends_with(')') || open == 0 {
        return false;
    }
    t[..open].chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        && t[..open].chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

struct Measured {
    stats: UpdateStats,
    update_ms: f64,
}

/// Apply one edit and require the updated graph to mean what a fresh build of
/// the same workspace means.
fn edit_and_compare(
    session: &mut Session,
    graph: &mut InstanceGraph,
    root: &str,
    path: &str,
    text: &str,
    label: &str,
) -> Measured {
    let res = session.update_file(path, text).expect("incremental update");
    assert_eq!(res.tier, UpdateTier::BodyOnly, "{label}: edit was not body-only");
    assert!(!res.reinferred.is_empty(), "{label}: a body-only edit reinferred nothing");
    let ws = session.workspace();
    let roots = instance_graph::emission_roots(ws, Some(root));
    let t = Instant::now();
    let stats = graph.update_bodies(ws, roots, &res.reinferred);
    let update_ms = t.elapsed().as_secs_f64() * 1000.0;

    let fresh = graph_of(ws, root);
    let d = canonical(ws, graph).diff(&canonical(ws, &fresh));
    assert!(d.is_empty(), "{label}: updated and fresh disagree:\n{}", d.join("\n"));
    Measured { stats, update_ms }
}

/// One project: build, delete a call, restore it, comparing after each.
fn project_case(rel: &str) {
    let dir = project_dir(rel);
    let files = project_files(&dir);
    assert!(files.len() > 1, "{rel} should have several modules");
    let mut session = Session::new(warm_stdlib(), files.clone())
        .unwrap_or_else(|e| panic!("{rel} session warm failed: {}", e.message));
    assert!(
        session.diagnostics().is_empty(),
        "{rel} does not typecheck clean, so nothing below would be evidence"
    );

    // MIN of several, not one sample: this box is noisy enough that a single
    // build has swung 2x between runs, and the comparison the note carries is
    // between phases rather than between machines.
    let mut fresh_ms = f64::MAX;
    let mut graph = graph_of(session.workspace(), &dir);
    for _ in 0..5 {
        let t = Instant::now();
        graph = graph_of(session.workspace(), &dir);
        fresh_ms = fresh_ms.min(t.elapsed().as_secs_f64() * 1000.0);
    }
    let owners_total = graph.contributions.len();
    let instances_total = graph.instances.len();

    // A candidate whose deletion actually STRANDS something. Most bare calls
    // are to a callee reached from elsewhere too, and a deletion that prunes
    // nothing would make the arm vacuous -- so the search is reported with its
    // denominator rather than stopping at the first line that parses.
    let mut examined = 0usize;
    let mut chosen: Option<(String, String, String)> = None;
    'outer: for (path, text) in &files {
        if !path.contains(&format!("{}/", rel.rsplit('/').next().unwrap_or(rel))) {
            continue;
        }
        for (i, line) in text.lines().enumerate() {
            if !is_bare_call_statement(line) {
                continue;
            }
            examined += 1;
            let edited: String = text
                .lines()
                .enumerate()
                .map(|(j, l)| if j == i { format!("// {l}") } else { l.to_string() })
                .collect::<Vec<_>>()
                .join("\n");
            let Ok(res) = session.update_file(path, &edited) else { continue };
            let ok = res.tier == UpdateTier::BodyOnly && session.diagnostics().is_empty();
            let prunes = if ok {
                let ws = session.workspace();
                let after = graph_of(ws, &dir);
                let before: BTreeSet<_> = graph.instances.iter().cloned().collect();
                before.difference(&after.instances).count() > 0
            } else {
                false
            };
            session.update_file(path, text).expect("revert");
            if prunes {
                chosen = Some((path.clone(), text.clone(), edited));
                break 'outer;
            }
        }
    }
    let (path, original, edited) = chosen.unwrap_or_else(|| {
        panic!("{rel}: no bare call statement whose deletion strands anything, out of {examined} examined")
    });

    // The same for the update: the first call pays for whatever the machine
    // was doing a moment ago.
    let del = edit_and_compare(&mut session, &mut graph, &dir, &path, &edited, "deletion");
    assert!(del.stats.pruned > 0, "{rel}: the chosen deletion pruned nothing");
    let add = edit_and_compare(&mut session, &mut graph, &dir, &path, &original, "addition");
    assert!(add.stats.newly_reached > 0, "{rel}: restoring the call reached nothing new");

    for (what, m) in [("delete", &del), ("add", &add)] {
        assert!(
            m.stats.expanded * 4 < m.stats.owners,
            "{rel} {what}: walked {} of {} owners -- a rebuild, not an update",
            m.stats.expanded,
            m.stats.owners
        );
        assert_eq!(m.stats.settle_rounds, 0, "{rel} {what}: the conformance fixpoint engaged");
    }

    println!(
        "{rel}: fresh build {fresh_ms:.1} ms over {owners_total} owners / {instances_total} instances\n  \
         delete: {:.2} ms, {} affected, {} walked, {} pruned, {} new -> {} owners / {} instances\n  \
         add:    {:.2} ms, {} affected, {} walked, {} pruned, {} new -> {} owners / {} instances\n  \
         candidates examined: {examined}, edited file: {path}",
        del.update_ms,
        del.stats.affected,
        del.stats.expanded,
        del.stats.pruned,
        del.stats.newly_reached,
        del.stats.owners,
        del.stats.instances,
        add.update_ms,
        add.stats.affected,
        add.stats.expanded,
        add.stats.pruned,
        add.stats.newly_reached,
        add.stats.owners,
        add.stats.instances,
    );
}

#[test]
fn match3_updater_matches_a_fresh_build() {
    project_case("games/match3");
}

#[test]
fn bitgun_updater_matches_a_fresh_build() {
    project_case("bitgun");
}

#[test]
fn mag_updater_matches_a_fresh_build() {
    project_case("mag");
}
