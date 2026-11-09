//! The `__SKULL__` test-result line protocol shared by the per-backend test
//! executors (luajit in `skull_backend`, node in `js_test`). One parser, one
//! histogram helper.

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
    /// Every `TIME` line: one per test the harness finished, in run order.
    pub times: Vec<TestTime>,
}

/// `__SKULL__\tTIME\t<name>\t<ms>\t<pass|fail|value>`: a test's wall time and
/// the harness-side result; a `value` result is judged by the host. The Skull
/// harness appends its run token and ordinal for watchdog progress tracking.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TestTime {
    pub name: String,
    pub ms: f64,
    pub result: String,
}

/// Parse the `__SKULL__\tPASS\t`, `__SKULL__\tFAIL\t<detail>` and
/// `__SKULL__\tVALUE\t<name>\t<serialized>` result lines and the
/// [`TestTime`] lines out of a test program's stdout. `None` = no valid result
/// protocol: either no result lines at all (the program never reached the
/// harness, an emit/load bug rather than a test failure) or a `__SKULL__` line
/// of an unknown kind or shape, which is logged. Every caller treats `None` as
/// a harness failure that shows the whole stdout; counting an unrecognized line
/// as a pass would let a broken harness report green.
pub(crate) fn parse_skull_markers(stdout: &str) -> Option<SkullMarkers> {
    let mut markers = SkullMarkers {
        passed: 0,
        fails: Vec::new(),
        values: Vec::new(),
        times: Vec::new(),
    };
    let mut results = 0usize;
    for line in stdout.lines() {
        let Some(body) = line.strip_prefix("__SKULL__\t") else {
            continue;
        };
        let (kind, rest) = body.split_once('\t').unwrap_or((body, ""));
        let parsed = match kind {
            "PASS" if rest.is_empty() && body.len() > kind.len() => {
                markers.passed += 1;
                true
            }
            "FAIL" if body.len() > kind.len() => {
                markers.fails.push(rest.to_string());
                true
            }
            "VALUE" => match rest.split_once('\t') {
                Some((name, value)) if !name.is_empty() => {
                    markers.values.push((name.to_string(), value.to_string()));
                    true
                }
                _ => false,
            },
            "TIME" => {
                let mut parts = rest.split('\t');
                let time = (|| {
                    Some(TestTime {
                        name: parts.next().filter(|n| !n.is_empty())?.to_string(),
                        ms: parts.next()?.parse().ok()?,
                        result: parts.next()?.to_string(),
                    })
                })();
                match time {
                    Some(time) => {
                        markers.times.push(time);
                        // TIME is bookkeeping, not a result.
                        continue;
                    }
                    None => false,
                }
            }
            _ => false,
        };
        if !parsed {
            log::error!(target: "CLI", "test protocol error: unrecognized marker line {line:?}");
            return None;
        }
        results += 1;
    }
    (results > 0).then_some(markers)
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
                    ));
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
    let mut rows: Vec<(String, usize)> = counts
        .into_iter()
        .map(|(r, c)| (r.to_string(), c))
        .collect();
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
    fn time_lines_are_parsed_and_not_counted_as_results() {
        let stdout = "__SKULL__\tPASS\t\n\
                      __SKULL__\tTIME\tt_a\t12.500\tpass\twatch-token\t0\n\
                      __SKULL__\tVALUE\tt_b\t42\n\
                      __SKULL__\tTIME\tt_b\t0.031\tvalue\n";
        let m = parse_skull_markers(stdout).expect("markers present");
        assert_eq!(m.passed, 1);
        assert_eq!(m.values.len(), 1);
        assert_eq!(
            m.times,
            vec![
                TestTime {
                    name: "t_a".into(),
                    ms: 12.5,
                    result: "pass".into()
                },
                TestTime {
                    name: "t_b".into(),
                    ms: 0.031,
                    result: "value".into()
                },
            ]
        );
    }

    #[test]
    fn unknown_or_malformed_result_marker_is_rejected_not_counted() {
        // An unknown kind used to be counted as a PASS (passed = results -
        // fails - values). Every malformed shape below must reject the run.
        for bad in [
            "__SKULL__\tSKIP\tt_a\n",
            "__SKULL__\tPASSED\t\n",
            "__SKULL__\tPASS\tdetail\n",
            "__SKULL__\tFAIL\n",
            "__SKULL__\tVALUE\tt_a\n",
            "__SKULL__\tTIME\tt_a\tnot-ms\tpass\n",
            "__SKULL__\tnote\tFAIL\tx\n",
        ] {
            let stdout = format!("__SKULL__\tPASS\t\n{bad}");
            assert!(parse_skull_markers(&stdout).is_none(), "accepted {bad:?}");
        }
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
