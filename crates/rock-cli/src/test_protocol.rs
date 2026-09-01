//! The `__SKULL__` test-result line protocol shared by the per-backend test
//! executors (luajit in `skull_backend`, node in `js_test`). One parser, one
//! histogram helper -- the executor seam of
//! `docs/backends.md`.

/// Parsed marker lines of one test-program run.
pub(crate) struct SkullMarkers {
    /// Number of `PASS` lines.
    pub passed: usize,
    /// The `<detail>` of every `FAIL` line.
    pub fails: Vec<String>,
    /// `(test name, serialized value)` of every `VALUE` line -- a
    /// `@test(returns = ...)` fn that ran without error; the HOST compares the
    /// value ([`compare_value`]) and turns it into pass/fail accounting.
    pub values: Vec<(String, String)>,
}

/// Parse the `__SKULL__\tPASS|FAIL\t<detail>` and
/// `__SKULL__\tVALUE\t<name>\t<serialized>` marker lines out of a test
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
    let values: Vec<(String, String)> = results
        .iter()
        .filter(|l| l.contains("\tVALUE\t"))
        .map(|l| {
            let mut parts = l.splitn(4, '\t').skip(2);
            let name = parts.next().unwrap_or("").to_string();
            let val = parts.next().unwrap_or("").to_string();
            (name, val)
        })
        .collect();
    Some(SkullMarkers {
        passed: results.len() - fails.len() - values.len(),
        fails,
        values,
    })
}

/// Default relative tolerance for `@test(returns = <float>)` comparison. An
/// explicit parameter on [`compare_value`] so a future executor with laxer
/// arithmetic (GLSL) can widen it; int/bool stay exact regardless.
pub(crate) const FLOAT_REL_TOL: f64 = 1e-12;

/// Compare a `VALUE` marker's serialized return value against the pinned
/// `@test(returns = ...)` literal. Backend-neutral: the host parses the
/// serialized value as f64 (or `true`/`false`) and compares numbers, so the
/// executors' serializations need not be byte-identical. Int expectations are
/// exact f64 equality (42 == 42.0); floats pass within `float_rel_tol`
/// relative tolerance; bools are exact. `Err` carries the FAIL detail.
pub(crate) fn compare_value(
    expect: &skull::ValueExpect,
    got: &str,
    float_rel_tol: f64,
) -> Result<(), String> {
    use skull::ValueExpect::*;
    match expect {
        Bool(b) => {
            let g = match got {
                "true" => true,
                "false" => false,
                _ => {
                    return Err(format!(
                        "value mismatch: expected <{b}> got unparseable <{got}>"
                    ))
                }
            };
            if g == *b {
                Ok(())
            } else {
                Err(format!("value mismatch: expected <{b}> got <{got}>"))
            }
        }
        Int(i) => {
            let g: f64 = got
                .parse()
                .map_err(|_| format!("value mismatch: expected <{i}> got unparseable <{got}>"))?;
            if g == *i as f64 {
                Ok(())
            } else {
                Err(format!("value mismatch: expected <{i}> got <{got}>"))
            }
        }
        Float(x) => {
            let g: f64 = got
                .parse()
                .map_err(|_| format!("value mismatch: expected <{x}> got unparseable <{got}>"))?;
            let within = g == *x || (g - x).abs() <= float_rel_tol * g.abs().max(x.abs());
            if within {
                Ok(())
            } else {
                Err(format!("value mismatch: expected <{x}> got <{got}>"))
            }
        }
    }
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

    #[test]
    fn parses_value_lines_alongside_pass_and_fail() {
        let stdout = "__SKULL__\tPASS\t\n\
                      __SKULL__\tVALUE\tt_a\t42\n\
                      __SKULL__\tFAIL\tt_b: error: boom\n\
                      __SKULL__\tVALUE\tt_c\t-1.5\n";
        let m = parse_skull_markers(stdout).expect("markers present");
        assert_eq!(m.passed, 1);
        assert_eq!(m.fails, vec!["t_b: error: boom".to_string()]);
        assert_eq!(
            m.values,
            vec![
                ("t_a".to_string(), "42".to_string()),
                ("t_c".to_string(), "-1.5".to_string())
            ]
        );
    }

    #[test]
    fn compare_int_exact_and_across_serializations() {
        use skull::ValueExpect::*;
        assert!(compare_value(&Int(42), "42", FLOAT_REL_TOL).is_ok());
        // A backend may serialize an int-typed 42 as 42.0 (or 17-digit form);
        // the host compares parsed f64s, not strings.
        assert!(compare_value(&Int(42), "42.000000000000000", FLOAT_REL_TOL).is_ok());
        let err = compare_value(&Int(43), "42", FLOAT_REL_TOL).unwrap_err();
        assert_eq!(err, "value mismatch: expected <43> got <42>");
    }

    #[test]
    fn compare_float_relative_tolerance() {
        use skull::ValueExpect::*;
        assert!(compare_value(&Float(1.0), "1.0000000000001", FLOAT_REL_TOL).is_ok());
        assert!(compare_value(&Float(0.0), "0", FLOAT_REL_TOL).is_ok());
        assert!(compare_value(&Float(-2.5), "-2.5", FLOAT_REL_TOL).is_ok());
        // Relative diff 1e-10 is OUTSIDE the default 1e-12 tolerance.
        assert!(compare_value(&Float(1.0), "1.0000000001", FLOAT_REL_TOL).is_err());
        // A widened tolerance (the future GLSL executor) admits it.
        assert!(compare_value(&Float(1.0), "1.0000000001", 1e-8).is_ok());
    }

    #[test]
    fn compare_bool_exact() {
        use skull::ValueExpect::*;
        assert!(compare_value(&Bool(true), "true", FLOAT_REL_TOL).is_ok());
        assert!(compare_value(&Bool(false), "false", FLOAT_REL_TOL).is_ok());
        assert!(compare_value(&Bool(true), "false", FLOAT_REL_TOL).is_err());
    }

    #[test]
    fn compare_unparseable_value_is_an_error() {
        use skull::ValueExpect::*;
        let err = compare_value(&Int(1), "abc", FLOAT_REL_TOL).unwrap_err();
        assert!(err.contains("unparseable <abc>"), "got: {err}");
        assert!(compare_value(&Bool(true), "1", FLOAT_REL_TOL).is_err());
    }
}
