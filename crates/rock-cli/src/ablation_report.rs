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

/// The status a census-printing exit leaves with.
///
/// Separate from [`exit_with_census`] because that one cannot be called from a
/// test: it ends the process. This is the whole decision, and the decision is
/// where the mode's invariant lives -- S 0e's fatality is carried by the EXIT
/// STATUS, so a run that recorded fatals may not leave with 0 no matter what
/// the call site asked for. Several call sites do ask for 0 (`rock stone` and
/// `rock watch` on a clean shutdown) or forward a subprocess's status, and
/// honouring that verbatim is exactly the "printed a census and exited 0" hole
/// this module's header forbids.
pub fn census_exit_code(requested: i32, had_fatals: bool) -> i32 {
    if had_fatals && requested == 0 {
        return 1;
    }
    requested
}

/// Exit the process, printing the census first.
///
/// `std::process::exit` runs no destructors and returns to no caller, so the
/// drain in `main` never happens on a path that calls it -- and the paths that
/// do are exactly the ones a build under the mode dies on. That is not a
/// cosmetic loss: the mode's contract is that it COLLECTS, so a census is
/// evidence about the whole run, and a project whose build died reported
/// NOTHING while a project that happened to compile reported a full taxonomy.
/// The `Iterator`-bounded surface is entirely in the first group, which is why
/// its category read zero everywhere.
pub fn exit_with_census(code: i32) -> ! {
    let had_fatals = report("whole run");
    crate::invocation_log::exit(census_exit_code(code, had_fatals))
}

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
    // Three units, because they answer three questions and quoting one for
    // another is how "578 pairs" became "578 declarations" in the S 0e summary:
    // pairs price the graph, sites price the corpus, callees price the SPEC work.
    let callees = ablation::callees_by_category(&fatals);
    println!("| category | distinct (callee x caller) | call sites | unique callees |");
    println!("|---|---:|---:|---:|");
    for (c, (d, n)) in &by_cat {
        println!("| `{}` | {d} | {n} | {} |", c.name(), callees.get(c).copied().unwrap_or(0));
    }
    println!();
    println!(
        "total: {} distinct pairs, {} call sites, {} unique callees ({} raw records)",
        distinct.len(),
        ablation::distinct_sites(&fatals),
        fatals.iter().map(|f| f.callee.as_str()).collect::<std::collections::BTreeSet<_>>().len(),
        fatals.len(),
    );
    println!();

    // Per category, the widest offenders first: a fn reached from 300 call sites
    // is one defect, and sorting by site count is what makes the biggest single
    // cause visible rather than the alphabetically first one.
    for (cat, _) in &by_cat {
        let mut rows: Vec<_> = distinct.iter().filter(|(c, ..)| c == cat).collect();
        rows.sort_by(|a, b| b.3.cmp(&a.3).then(a.1.cmp(&b.1)));
        println!("### `{}`", cat.name());
        println!();
        // A category is not a mechanism. `unsupported-surface` holds both the
        // `Iterator`-bounded splice gap and the partial-key gap, and pricing the
        // retained mechanisms one by one (S 0e's verdict table) needs them apart.
        // The detail's first clause is the discriminant it is required to state.
        type MechRow<'a> = (
            std::collections::BTreeSet<(&'a str, &'a str)>,
            std::collections::BTreeSet<(ustr::Ustr, (usize, usize))>,
            std::collections::BTreeSet<&'a str>,
        );
        let mut mech: std::collections::BTreeMap<String, MechRow> = Default::default();
        let mut payloads: std::collections::BTreeMap<String, std::collections::BTreeSet<&str>> =
            Default::default();
        for f in fatals.iter().filter(|f| f.category == *cat) {
            let head = f.detail.split(':').next().unwrap_or(&f.detail);
            // A detail names the offending TYPE, so grouping on the raw text
            // splits one mechanism into one row per instantiation. Elide the
            // payload for the key and list the payloads separately -- both are
            // wanted, and only one of them is the mechanism.
            let key = elide_payloads(head);
            for p in head.split('`').skip(1).step_by(2) {
                payloads.entry(key.clone()).or_default().insert(p);
            }
            let e = mech.entry(key).or_default();
            e.0.insert((f.callee.as_str(), f.caller.as_str()));
            e.1.insert((f.span.file, f.span.offset_range));
            e.2.insert(f.callee.as_str());
        }
        println!("| mechanism | pairs | sites | callees |");
        println!("|---|---:|---:|---:|");
        for (head, (pairs, sites, callees)) in &mech {
            println!("| {head} | {} | {} | {} |", pairs.len(), sites.len(), callees.len());
        }
        println!();
        for (head, (_, _, callees)) in &mech {
            println!("- {head}");
            println!("  callees: {}", callees.iter().copied().collect::<Vec<_>>().join(", "));
            if let Some(ps) = payloads.get(head) {
                println!("  payloads: {}", ps.iter().copied().collect::<Vec<_>>().join(", "));
            }
        }
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

/// Replace every `` `...` `` payload with `` `_` ``. Two records of one
/// mechanism differ only by the type they name, so the elided form is the
/// mechanism and the payloads are its instantiations.
fn elide_payloads(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut inside = false;
    for c in s.chars() {
        if c == '`' {
            out.push('`');
            if inside {
                inside = false;
            } else {
                out.push('_');
                inside = true;
            }
        } else if !inside {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::census_exit_code;

    /// S 0e's fatality is carried by the exit status, so a recorded fatal must
    /// override a success the call site asked for. `rock stone` and `rock watch`
    /// exit 0 on a clean shutdown and `rock compile` forwards a subprocess's
    /// status; without this a run under the mode prints its whole taxonomy and
    /// still reports success, which is the one thing the mode exists to prevent.
    #[test]
    fn a_recorded_fatal_overrides_a_zero_exit() {
        assert_eq!(census_exit_code(0, true), 1, "fatals must not exit 0");
    }

    /// The three neighbours, so the arm above is a rule and not a constant.
    /// A clean run keeps its 0, and a non-zero status is passed through
    /// unchanged either way -- overriding THAT would relabel a subprocess's
    /// exit code and lose which failure it was.
    #[test]
    fn every_other_combination_passes_the_requested_status_through() {
        assert_eq!(census_exit_code(0, false), 0, "a clean run keeps its success");
        assert_eq!(census_exit_code(1, true), 1, "an already-failing run is unchanged");
        assert_eq!(census_exit_code(2, true), 2, "a fatal must not relabel a status");
        assert_eq!(census_exit_code(2, false), 2, "no fatals must not relabel a status");
    }
}
