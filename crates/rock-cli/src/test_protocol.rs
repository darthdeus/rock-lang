//! The `__SKULL__` test-result line protocol shared by the per-backend test
//! executors (luajit in `skull_backend`, node in `js_test`). One parser, one
//! histogram helper -- the executor seam of
//! `docs/multi-backend-testing-plan.md`.

/// Parsed marker lines of one test-program run.
pub(crate) struct SkullMarkers {
    /// Number of `PASS` lines.
    pub passed: usize,
    /// The `<detail>` of every `FAIL` line.
    pub fails: Vec<String>,
}

/// Parse the `__SKULL__\tPASS|FAIL\t<detail>` marker lines out of a test
/// program's stdout. `None` = no marker lines at all: the program never reached
/// the harness (an emit/load bug, not a test failure).
pub(crate) fn parse_skull_markers(stdout: &str) -> Option<SkullMarkers> {
    let results: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("__SKULL__\t"))
        .collect();
    if results.is_empty() {
        return None;
    }
    let fails: Vec<String> = results
        .iter()
        .filter(|l| l.contains("\tFAIL\t"))
        .map(|l| l.splitn(3, '\t').nth(2).unwrap_or("").to_string())
        .collect();
    Some(SkullMarkers { passed: results.len() - fails.len(), fails })
}

/// Bucket reason strings into a histogram sorted by count desc, ties by reason
/// asc (deterministic output; the JS run's skip histogram IS the emitter work
/// list, so ordering must be stable).
pub(crate) fn reason_histogram<'a>(
    reasons: impl IntoIterator<Item = &'a str>,
) -> Vec<(String, usize)> {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for r in reasons {
        *counts.entry(r).or_insert(0) += 1;
    }
    let mut rows: Vec<(String, usize)> =
        counts.into_iter().map(|(r, c)| (r.to_string(), c)).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pass_and_fail_lines_ignoring_other_output() {
        let stdout = "hello\n\
                      __SKULL__\tPASS\t\n\
                      noise __SKULL__ not at line start\n\
                      __SKULL__\tFAIL\tt_a: error: boom\n\
                      __SKULL__\tPASS\t\n\
                      __SKULL__\tFAIL\tt_b: output mismatch: expected <1> got <2>\n";
        let m = parse_skull_markers(stdout).expect("markers present");
        assert_eq!(m.passed, 2);
        assert_eq!(
            m.fails,
            vec![
                "t_a: error: boom".to_string(),
                "t_b: output mismatch: expected <1> got <2>".to_string()
            ]
        );
    }

    #[test]
    fn no_marker_lines_is_none() {
        assert!(parse_skull_markers("plain output\nno markers here\n").is_none());
        assert!(parse_skull_markers("").is_none());
    }

    #[test]
    fn fail_detail_preserves_tabs_past_the_marker() {
        // splitn(3, '\t') keeps tabs INSIDE the detail intact.
        let m = parse_skull_markers("__SKULL__\tFAIL\ta: got <x\ty>\n").expect("markers");
        assert_eq!(m.passed, 0);
        assert_eq!(m.fails, vec!["a: got <x\ty>".to_string()]);
    }

    #[test]
    fn histogram_sorts_by_count_desc_then_reason_asc() {
        let reasons = ["b", "a", "b", "c", "a", "b"];
        assert_eq!(
            reason_histogram(reasons),
            vec![
                ("b".to_string(), 3),
                ("a".to_string(), 2),
                ("c".to_string(), 1)
            ]
        );
        // Tie: alphabetical.
        assert_eq!(
            reason_histogram(["z", "y"]),
            vec![("y".to_string(), 1), ("z".to_string(), 1)]
        );
    }

    #[test]
    fn histogram_of_nothing_is_empty() {
        assert!(reason_histogram([]).is_empty());
    }
}
