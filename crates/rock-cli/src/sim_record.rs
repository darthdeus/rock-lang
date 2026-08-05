//! Structured per-entry sim results and the differential comparator that
//! decides whether two runs of the same sim are observationally equal.
//!
//! This is the Phase B oracle from `docs/sim-speed-plan.md`: a sim run in a
//! fresh process and the same sim run after a predecessor in ONE process must
//! produce identical records. Both sides are built by the same code from the
//! same `SimSummary`, so the two schemas cannot drift.
//!
//! The comparator reports a per-arm census as well as the differences. A
//! comparison that found nothing is only evidence when each arm actually had
//! something to compare -- zero over zero is not agreement.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::Path;

pub use crate::run_sim::SiteMessage;

/// Every arm the comparator looks at, in report order. A test asserting on a
/// specific arm goes through [`Comparison::population`], which fails loudly
/// when the arm is absent -- so deleting an arm from `compare` cannot pass
/// silently.
pub const COMPARED_ARMS: &[&str] = &[
    "verdict",
    "expects_pass",
    "expects_fail",
    "errors",
    "saw_stop",
    "timed_out",
    "exit_code",
    "entry_diagnostics",
    "artifacts",
];

/// One file produced under an entry's `$OUT`, keyed in the manifest by its
/// path relative to that directory.
///
/// The digest is a non-cryptographic content hash: this compares two manifests
/// computed by one process in one run, not a trust boundary. Length is kept
/// alongside it so a report can say how the file differed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactEntry {
    pub len: u64,
    pub digest: u64,
}

/// The complete observable result of one sim entry.
#[derive(Clone, Debug, Default)]
pub struct SimRecord {
    pub name: String,
    pub seq: usize,
    pub expects_pass: Vec<SiteMessage>,
    pub expects_fail: Vec<SiteMessage>,
    pub errors: Vec<SiteMessage>,
    pub saw_stop: bool,
    pub timed_out: bool,
    /// `None` when the process exited 0. A worker's exit status is attributed
    /// to the last entry that started, which is where the process died.
    pub exit_code: Option<i32>,
    /// Everything the child wrote for this entry, boot and teardown noise
    /// included. The debugging stream: RECORDED but not compared, because a
    /// worker's first entry legitimately carries a process boot log that later
    /// entries do not.
    pub diagnostics: String,
    /// The child's own stderr emitted strictly BETWEEN this entry's start and
    /// end, normalized by [`normalize_diagnostics`]. This IS compared: it is
    /// the only place a warning suppressed by a predecessor's logger-dedup
    /// state becomes visible, and dedup is exactly a state class the worker
    /// must reset.
    pub entry_diagnostics: String,
    pub artifacts: BTreeMap<String, ArtifactEntry>,
}

impl SimRecord {
    /// The pass/fail word, derived rather than stored so a record cannot
    /// disagree with the fields it was built from.
    pub fn verdict(&self) -> &'static str {
        if !self.timed_out
            && self.exit_code.is_none()
            && self.saw_stop
            && self.expects_fail.is_empty()
            && self.errors.is_empty()
        {
            "PASS"
        } else {
            "FAIL"
        }
    }
}

/// How much of an arm was actually compared. Zero means the fixture never
/// exercised it, so its agreement proves nothing.
#[derive(Clone, Debug)]
pub struct ArmCensus {
    pub arm: &'static str,
    pub population: usize,
}

#[derive(Clone, Debug)]
pub struct Difference {
    pub arm: &'static str,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct Comparison {
    pub arms: Vec<ArmCensus>,
    pub differences: Vec<Difference>,
}

impl Comparison {
    pub fn is_equivalent(&self) -> bool {
        self.differences.is_empty()
    }

    /// Compared population for `arm`. Panics when the arm was not compared at
    /// all -- that is a comparator regression, not a data point, and a test
    /// asking about an arm must not silently read it as "nothing to see".
    pub fn population(&self, arm: &str) -> usize {
        match self.arms.iter().find(|a| a.arm == arm) {
            Some(a) => a.population,
            None => panic!(
                "arm `{arm}` was not compared; comparator arms are {:?}",
                self.arms.iter().map(|a| a.arm).collect::<Vec<_>>()
            ),
        }
    }

    pub fn differs_on(&self, arm: &str) -> bool {
        self.differences.iter().any(|d| d.arm == arm)
    }

    /// One-line-per-difference report for a test failure message.
    pub fn report(&self) -> String {
        self.differences
            .iter()
            .map(|d| format!("  [{}] {}", d.arm, d.detail))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn describe(msgs: &[SiteMessage]) -> Vec<String> {
    msgs.iter()
        .map(|m| {
            if m.site.is_empty() {
                m.message.clone()
            } else {
                format!("{}: {}", m.site, m.message)
            }
        })
        .collect()
}

/// Compare an isolated (fresh-process) record against a worker record for the
/// same sim. `isolated` is the reference; any divergence is the worker's.
pub fn compare(isolated: &SimRecord, worker: &SimRecord) -> Comparison {
    let mut arms = Vec::new();
    let mut differences = Vec::new();
    let mut arm = |name: &'static str, population: usize| {
        arms.push(ArmCensus { arm: name, population });
    };

    arm("verdict", 2);
    if isolated.verdict() != worker.verdict() {
        differences.push(Difference {
            arm: "verdict",
            detail: format!("isolated {} vs worker {}", isolated.verdict(), worker.verdict()),
        });
    }

    for (name, a, b) in [
        ("expects_pass", &isolated.expects_pass, &worker.expects_pass),
        ("expects_fail", &isolated.expects_fail, &worker.expects_fail),
        ("errors", &isolated.errors, &worker.errors),
    ] {
        arm(name, a.len() + b.len());
        let (da, db) = (describe(a), describe(b));
        if da != db {
            differences.push(Difference {
                arm: name,
                detail: format!("isolated {da:?} vs worker {db:?}"),
            });
        }
    }

    arm("saw_stop", 2);
    if isolated.saw_stop != worker.saw_stop {
        differences.push(Difference {
            arm: "saw_stop",
            detail: format!("isolated {} vs worker {}", isolated.saw_stop, worker.saw_stop),
        });
    }

    arm("timed_out", 2);
    if isolated.timed_out != worker.timed_out {
        differences.push(Difference {
            arm: "timed_out",
            detail: format!("isolated {} vs worker {}", isolated.timed_out, worker.timed_out),
        });
    }

    arm("exit_code", 2);
    if isolated.exit_code != worker.exit_code {
        differences.push(Difference {
            arm: "exit_code",
            detail: format!("isolated {:?} vs worker {:?}", isolated.exit_code, worker.exit_code),
        });
    }

    let iso_diag: Vec<&str> = isolated.entry_diagnostics.lines().collect();
    let wrk_diag: Vec<&str> = worker.entry_diagnostics.lines().collect();
    arm("entry_diagnostics", iso_diag.len() + wrk_diag.len());
    if iso_diag != wrk_diag {
        differences.push(Difference {
            arm: "entry_diagnostics",
            detail: format!("isolated {iso_diag:?} vs worker {wrk_diag:?}"),
        });
    }

    let mut paths: Vec<&String> = isolated.artifacts.keys().chain(worker.artifacts.keys()).collect();
    paths.sort();
    paths.dedup();
    arm("artifacts", paths.len());
    for path in paths {
        match (isolated.artifacts.get(path), worker.artifacts.get(path)) {
            (Some(a), Some(b)) if a == b => {}
            (Some(a), Some(b)) => differences.push(Difference {
                arm: "artifacts",
                detail: format!(
                    "`{path}` differs: isolated {}B/{:016x} vs worker {}B/{:016x}",
                    a.len, a.digest, b.len, b.digest
                ),
            }),
            (Some(_), None) => differences.push(Difference {
                arm: "artifacts",
                detail: format!("`{path}` produced only by the isolated run"),
            }),
            (None, Some(_)) => differences.push(Difference {
                arm: "artifacts",
                detail: format!("`{path}` produced only by the worker run"),
            }),
            (None, None) => unreachable!("path came from one of the two maps"),
        }
    }

    Comparison { arms, differences }
}

/// Manifest of everything under `dir`, keyed by path relative to it.
///
/// Called while the entry's `$OUT` is still live -- after deferred artifact
/// processing (video assertions), before `cleanup_path` removals -- so it sees
/// what the entry actually produced rather than what survived teardown.
///
/// Fails CLOSED. A skipped directory entry or an unreadable file would drop
/// evidence from the manifest, and two runs that both dropped the same evidence
/// compare equal -- the manifest would then certify an equivalence it never
/// measured. Every failure is an error instead.
pub fn manifest_of_dir(dir: &Path) -> anyhow::Result<BTreeMap<String, ArtifactEntry>> {
    let mut out = BTreeMap::new();
    for entry in walkdir::WalkDir::new(dir).sort_by_file_name() {
        let entry = entry
            .map_err(|e| anyhow::anyhow!("walking `{}` for the artifact manifest: {e}", dir.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(dir).map_err(|e| {
            anyhow::anyhow!("`{}` is not under `{}`: {e}", entry.path().display(), dir.display())
        })?;
        let bytes = std::fs::read(entry.path())
            .map_err(|e| anyhow::anyhow!("reading `{}` for the artifact manifest: {e}", entry.path().display()))?;
        let mut hasher = rustc_hash::FxHasher::default();
        bytes.hash(&mut hasher);
        out.insert(
            rel.to_string_lossy().replace('\\', "/"),
            ArtifactEntry { len: bytes.len() as u64, digest: hasher.finish() },
        );
    }
    Ok(out)
}

/// Normalize one entry's raw child stderr so two runs of the same sim produce
/// the same text.
///
/// Two rewrites only: the logger's leading `[   0.123s LEVEL ]` timestamp is
/// removed, and the entry's `$OUT` path becomes a literal `$OUT`. Everything
/// else -- including every digit in the message -- is preserved. Numbers in a
/// log record are frame counters, entity ids, resource handles and error
/// payloads, which is to say they are exactly the leaked state this arm exists
/// to catch; a run whose warning says `gate 7` where the reference says
/// `gate 0` has diverged.
pub fn normalize_diagnostics(lines: &[String], out_dir: &Path) -> String {
    let out_str = out_dir.to_string_lossy().to_string();
    let mut result = String::new();
    for line in lines {
        let body = strip_log_stamp(line);
        let body = if out_str.is_empty() {
            body
        } else {
            &body.replace(&out_str, "$OUT")
        };
        result.push_str(body.trim_end());
        result.push('\n');
    }
    result
}

/// The five level tags `rock_log` emits, each exactly five characters
/// (`crates/rock-log/src/lib.rs`, `level_tag`). WARN and INFO are space-padded
/// to that width, which is why the closing bracket can look detached.
const LOG_LEVEL_TAGS: [&str; 5] = ["ERROR", "WARN ", "INFO ", "DEBUG", "TRACE"];
const LOG_TIME_WIDTH: usize = 8;
const LOG_TIME_PRECISION: usize = 3;

/// Remove a `rock_log` stamp, or return the line untouched.
///
/// Matches the emitter's grammar exactly -- `[{:>8.3}s {}] ` -- rather than
/// sniffing for something bracketed: leading padding, at least one integer
/// digit, a point, EXACTLY three fractional digits, `s`, one space, one of
/// [`LOG_LEVEL_TAGS`], and `]`. A loose match eats a game's own bracketed
/// prefix and erases the category this arm needs to tell records apart -- a
/// line like `[1.23s SCORE] result` is game output, not a log stamp, and both
/// its fractional precision and its tag say so.
fn strip_log_stamp(line: &str) -> &str {
    let Some(rest) = line.strip_prefix('[') else {
        return line;
    };
    let padded = rest.trim_start_matches(' ');
    let padding = rest.len() - padded.len();
    let int_digits = padded
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(padded.len());
    if int_digits == 0 {
        return line;
    }
    let Some(frac) = padded[int_digits..].strip_prefix('.') else {
        return line;
    };
    if frac.len() < LOG_TIME_PRECISION
        || !frac.as_bytes()[..LOG_TIME_PRECISION]
            .iter()
            .all(u8::is_ascii_digit)
    {
        return line;
    }
    let number_width = int_digits + 1 + LOG_TIME_PRECISION;
    if padding != LOG_TIME_WIDTH.saturating_sub(number_width) {
        return line;
    }
    // A fourth fractional digit fails this, which is what pins the precision.
    let Some(tail) = frac[LOG_TIME_PRECISION..].strip_prefix("s ") else {
        return line;
    };
    let Some(tail) = LOG_LEVEL_TAGS.iter().find_map(|tag| tail.strip_prefix(tag)) else {
        return line;
    };
    let Some(body) = tail.strip_prefix("] ") else {
        return line;
    };
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(site: &str, message: &str) -> SiteMessage {
        SiteMessage { message: message.into(), site: site.into() }
    }

    fn passing(name: &str) -> SimRecord {
        SimRecord {
            name: name.into(),
            seq: 0,
            expects_pass: vec![msg("f:1", "a")],
            saw_stop: true,
            ..SimRecord::default()
        }
    }

    #[test]
    fn identical_records_agree_on_every_arm() {
        let c = compare(&passing("x"), &passing("x"));
        assert!(c.is_equivalent(), "{}", c.report());
        // Every arm must be PRESENT in the census; whether it had anything to
        // compare depends on the fixture, which is the integration suite's
        // job (`tests/sim_worker_differential.rs`).
        for arm in COMPARED_ARMS {
            let _ = c.population(arm);
        }
        assert!(c.population("expects_pass") > 0, "this record does assert something");
    }

    /// Every arm must both be CENSUSED and actually COMPARED. The census alone
    /// only catches deleting an arm outright; disabling its comparison while
    /// leaving `arm(...)` in place would stay green without this. One synthetic
    /// mutation per arm, each differing from the baseline in that arm only.
    #[test]
    fn every_arm_reports_a_disagreement_confined_to_itself() {
        let mutations: Vec<(&str, fn(&mut SimRecord))> = vec![
            // `verdict` is derived, so the only way to move it alone is via a
            // field no other arm reads -- there is none, so it is driven here
            // by saw_stop and asserted together with it below.
            ("expects_pass", |r| r.expects_pass = vec![msg("f:1", "different")]),
            ("expects_fail", |r| r.expects_fail = vec![msg("f:2", "boom")]),
            ("errors", |r| r.errors = vec![msg("f:3", "runner error")]),
            ("saw_stop", |r| r.saw_stop = false),
            ("timed_out", |r| r.timed_out = true),
            ("exit_code", |r| r.exit_code = Some(3)),
            ("entry_diagnostics", |r| r.entry_diagnostics = "[MAG] a warning\n".into()),
            ("artifacts", |r| {
                r.artifacts.insert("f.txt".into(), ArtifactEntry { len: 1, digest: 1 });
            }),
        ];

        for (arm, mutate) in mutations {
            let mut worker = passing("x");
            mutate(&mut worker);
            let c = compare(&passing("x"), &worker);
            assert!(
                c.differs_on(arm),
                "arm `{arm}` is censused but not compared: a record differing only in \
                 `{arm}` reported {:?}",
                c.differences.iter().map(|d| d.arm).collect::<Vec<_>>()
            );
            assert!(
                c.population(arm) > 0,
                "arm `{arm}` reported a disagreement over an empty population"
            );
        }
    }

    /// `verdict` is derived from the other fields, so it cannot be moved on its
    /// own; what it owes is that it MOVES at all when the outcome changes.
    #[test]
    fn verdict_disagrees_whenever_the_outcome_does() {
        for mutate in [
            (|r: &mut SimRecord| r.saw_stop = false) as fn(&mut SimRecord),
            |r: &mut SimRecord| r.timed_out = true,
            |r: &mut SimRecord| r.exit_code = Some(1),
            |r: &mut SimRecord| r.expects_fail = vec![msg("f:2", "boom")],
            |r: &mut SimRecord| r.errors = vec![msg("f:3", "err")],
        ] {
            let mut worker = passing("x");
            mutate(&mut worker);
            let c = compare(&passing("x"), &worker);
            assert!(c.differs_on("verdict"), "verdict stayed put: {}", c.report());
        }
    }

    #[test]
    fn normalization_drops_the_stamp_and_the_tempdir_and_nothing_else() {
        let out = Path::new("/tmp/sim-abc123");
        let a = normalize_diagnostics(
            &[
                "[   0.789s WARN ] [MAG] gate 0: no member spawners".to_string(),
                "[   1.004s INFO ] wrote /tmp/sim-abc123/count.txt".to_string(),
            ],
            out,
        );
        // Same records, different elapsed times: the stamp is the only timing
        // the arm forgives.
        let b = normalize_diagnostics(
            &[
                "[  12.010s WARN ] [MAG] gate 0: no member spawners".to_string(),
                "[  13.900s INFO ] wrote /tmp/sim-abc123/count.txt".to_string(),
            ],
            out,
        );
        assert_eq!(a, b, "the elapsed-time stamp must not create a false difference");
        assert!(a.contains("$OUT/count.txt"), "the tempdir path is rewritten: {a}");
    }

    /// Digits inside a message are frame counters, entity ids, resource handles
    /// and error payloads -- leaked state, not noise. Collapsing them would
    /// blind the arm to the divergences it exists to report.
    #[test]
    fn a_message_digit_is_a_difference() {
        let out = Path::new("/tmp/sim-abc123");
        let zero = normalize_diagnostics(
            &["[   0.789s WARN ] [MAG] gate 0: no member spawners".to_string()],
            out,
        );
        let seven = normalize_diagnostics(
            &["[   0.789s WARN ] [MAG] gate 7: no member spawners".to_string()],
            out,
        );
        assert_ne!(zero, seven, "`gate 0` and `gate 7` are not the same record");
    }

    /// Every shape `rock_log` actually emits must lose its stamp and keep the
    /// rest -- including the padded `WARN `/`INFO ` tags, whose padding makes
    /// the closing bracket look detached.
    #[test]
    fn every_real_log_stamp_is_stripped() {
        let out = Path::new("");
        for (line, expected) in [
            ("[   0.789s INFO ] [MAG] hi", "[MAG] hi"),
            ("[   1.296s ERROR] [SLOWFRAME] frame 60 took 69.7ms", "[SLOWFRAME] frame 60 took 69.7ms"),
            ("[   0.001s WARN ] [SIM] careful", "[SIM] careful"),
            ("[  12.500s DEBUG] [ENGINE] x", "[ENGINE] x"),
            ("[1234.567s TRACE] no padding, wide number", "no padding, wide number"),
            ("[   0.000s INFO ] ", ""),
        ] {
            assert_eq!(
                normalize_diagnostics(&[line.to_string()], out),
                format!("{expected}\n"),
                "`{line}` is a real logger stamp and must be stripped"
            );
        }
    }

    /// The stamp is parsed, not sniffed. Everything here merely RESEMBLES one,
    /// and erasing any of it would destroy game output -- `[1.23s SCORE]` is
    /// the canonical trap: bracketed, numeric, ends in `s`, and is not a log
    /// record. Both its fractional precision and its tag rule it out.
    #[test]
    fn only_a_real_log_stamp_is_stripped() {
        let out = Path::new("");
        for line in [
            "[MAG] gate: no spawners",
            "[1.23s SCORE] result",
            "[   0.789s SCORE] a well-formed time with a tag we never emit",
            "[   0.789s Info ] the tags are upper case",
            "[   0.78s INFO ] two fractional digits",
            "[   0.7891s INFO ] four fractional digits",
            "[   0.789 INFO ] no seconds marker",
            "[   0.789sINFO ] no space after the seconds",
            "[   0.789s INFO] the tag is padded, so this is one char short",
            "[    0.789s INFO ] too much width padding",
            "[  0.789s INFO ] too little width padding",
            "[   0.789s ERROR]no separator after the bracket",
            "[   .789s INFO ] no integer digits",
            "[   0.789s] no level at all",
            "[0.5] a bracketed number that is not a stamp",
            "not bracketed at all",
            "",
        ] {
            assert_eq!(
                normalize_diagnostics(&[line.to_string()], out),
                format!("{line}\n"),
                "`{line}` is not a logger stamp and must survive intact"
            );
        }
    }

    /// A record that did not fire at all is the logger-dedup signal.
    #[test]
    fn a_suppressed_record_is_a_difference() {
        let out = Path::new("/tmp/sim-abc123");
        let both = normalize_diagnostics(
            &[
                "[   0.789s WARN ] [B0] fixture warning".to_string(),
                "[   1.004s INFO ] done".to_string(),
            ],
            out,
        );
        let suppressed = normalize_diagnostics(&["[   1.004s INFO ] done".to_string()], out);
        assert_ne!(both, suppressed);
    }

    #[test]
    fn a_manifest_over_an_unreadable_directory_is_an_error_not_an_empty_map() {
        let missing = Path::new("/nonexistent-rock-sim-out-dir-for-tests");
        assert!(
            manifest_of_dir(missing).is_err(),
            "a manifest that cannot be collected must not read as `produced nothing`"
        );
    }

    #[test]
    fn a_different_passing_expect_is_a_difference_not_a_matching_count() {
        // Two runs can pass the same NUMBER of expects while passing different
        // ones; a count-only record would call that equivalent.
        let mut worker = passing("x");
        worker.expects_pass = vec![msg("f:1", "b")];
        let c = compare(&passing("x"), &worker);
        assert!(c.differs_on("expects_pass"), "{c:?}");
        assert_eq!(c.population("expects_pass"), 2);
    }

    #[test]
    fn artifact_contents_are_compared_not_just_names() {
        let mut isolated = passing("x");
        isolated
            .artifacts
            .insert("count.txt".into(), ArtifactEntry { len: 1, digest: 7 });
        let mut worker = passing("x");
        worker
            .artifacts
            .insert("count.txt".into(), ArtifactEntry { len: 1, digest: 9 });

        let c = compare(&isolated, &worker);
        assert!(c.differs_on("artifacts"), "{c:?}");
        assert_eq!(c.population("artifacts"), 1);
    }

    #[test]
    fn a_missing_arm_is_a_loud_failure_not_a_quiet_zero() {
        let c = compare(&passing("x"), &passing("x"));
        assert!(std::panic::catch_unwind(|| c.population("no_such_arm")).is_err());
    }

    #[test]
    fn verdict_follows_the_fields_it_was_built_from() {
        let mut r = passing("x");
        assert_eq!(r.verdict(), "PASS");
        r.saw_stop = false;
        assert_eq!(r.verdict(), "FAIL", "an abort fails no expect -- that is the trap");
        r.saw_stop = true;
        r.exit_code = Some(2);
        assert_eq!(r.verdict(), "FAIL");
        r.exit_code = None;
        r.timed_out = true;
        assert_eq!(r.verdict(), "FAIL");
    }

    #[test]
    fn manifest_keys_are_relative_and_content_sensitive() {
        let dir = tempfile::TempDir::new().expect("tmp");
        std::fs::create_dir_all(dir.path().join("nested")).expect("mkdir");
        std::fs::write(dir.path().join("a.txt"), "one").expect("write");
        std::fs::write(dir.path().join("nested/b.txt"), "two").expect("write");

        let first = manifest_of_dir(dir.path()).expect("manifest");
        assert_eq!(
            first.keys().cloned().collect::<Vec<_>>(),
            vec!["a.txt".to_string(), "nested/b.txt".to_string()]
        );

        std::fs::write(dir.path().join("a.txt"), "ONE").expect("rewrite");
        let second = manifest_of_dir(dir.path()).expect("manifest");
        assert_eq!(first["nested/b.txt"], second["nested/b.txt"]);
        assert_ne!(
            first["a.txt"], second["a.txt"],
            "same length, different bytes must not compare equal"
        );
    }
}
