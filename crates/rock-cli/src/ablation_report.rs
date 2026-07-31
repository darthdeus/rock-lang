//! Stage-0e ablation census reporting (`docs/monomorphization-plan.md` S 0e).
//!
//! The mode COLLECTS fatals rather than aborting at the first one (see
//! `skull_mir::ablation`), so something has to drain the registry and turn it
//! into the taxonomy the stage owes. That is here: one report, printed after the
//! command's own work, followed by a non-zero exit.
//!
//! The exit status is what makes this an ablation rather than a lint: S 0e says
//! lowering or emission of an unspecialized generic Rock body is FATAL, and a
//! run that printed a census and exited 0 would let a green suite hide it.

use skull_mir::ablation::{self, FatalCategory};

/// Print the census. Returns `true` when at least one fatal was recorded, which
/// the caller turns into a non-zero exit.
///
/// `label` names what was built, so a multi-project driver can attribute a
/// census to its project.
pub fn report(label: &str) -> bool {
    if !ablation::enabled() {
        return false;
    }
    let fatals = ablation::take();
    let distinct = ablation::distinct(&fatals);

    println!();
    println!("## Stage-0e totality ablation: {label}");
    println!();
    let examined = ablation::examined();
    if fatals.is_empty() {
        // A zero numerator over a zero denominator is not a clean build. Say
        // which one it is: a run that resolved no project compiles nothing and
        // would otherwise print the clean message.
        if examined == 0 {
            println!(
                "NO GENERIC ROCK CALL WAS EXAMINED -- nothing was compiled under the mode, \
                 so this run is not evidence either way."
            );
        } else {
            println!(
                "no fatals over {examined} generic Rock call(s) examined. Every reachable \
                 generic call named a complete instantiation, with inlining disabled."
            );
        }
        return false;
    }
    println!("generic Rock calls examined: {examined}");
    println!();

    let mut by_cat: std::collections::BTreeMap<FatalCategory, (usize, usize)> =
        Default::default();
    for (c, _, _, n, _) in &distinct {
        let e = by_cat.entry(*c).or_insert((0, 0));
        e.0 += 1;
        e.1 += n;
    }
    println!("| category | distinct (callee x caller) | call sites |");
    println!("|---|---:|---:|");
    for (c, (d, n)) in &by_cat {
        println!("| `{}` | {d} | {n} |", c.name());
    }
    println!();
    println!("total: {} distinct, {} call sites", distinct.len(), fatals.len());
    println!();

    // Per category, the widest offenders first: a fn reached from 300 call sites
    // is one defect, and sorting by site count is what makes the biggest single
    // cause visible rather than the alphabetically first one.
    for (cat, _) in &by_cat {
        let mut rows: Vec<_> = distinct.iter().filter(|(c, ..)| c == cat).collect();
        rows.sort_by(|a, b| b.3.cmp(&a.3).then(a.1.cmp(&b.1)));
        println!("### `{}`", cat.name());
        println!();
        let detail = fatals
            .iter()
            .find(|f| f.category == *cat)
            .map(|f| f.detail.clone())
            .unwrap_or_default();
        println!("{detail}");
        println!();
        println!("| callee | caller | sites | span |");
        println!("|---|---|---:|---|");
        // `ROCK_MONO_ABLATION_FULL=1` prints every row. The default cap hides
        // whether a category has members beyond its widest ones, which is
        // exactly the question "is this category ONLY the iter witness?" asks.
        let cap = if std::env::var("ROCK_MONO_ABLATION_FULL").is_ok() { usize::MAX } else { 15 };
        for (_, callee, caller, n, span) in rows.iter().take(cap) {
            let file = span.file.as_str();
            let file = file.rsplit_once("rock-mono/").map(|(_, r)| r).unwrap_or(file);
            println!("| `{callee}` | `{caller}` | {n} | {file}:{} |", span.line_range.0 + 1);
        }
        if rows.len() > cap {
            println!();
            println!("...and {} more distinct pairs.", rows.len() - cap);
        }
        println!();
    }
    true
}
