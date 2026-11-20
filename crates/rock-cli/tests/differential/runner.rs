use crate::support::{repo_root, rock};
use std::fmt;
use std::io::{Read, Seek};
use std::ops::Range;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use wait_timeout::ChildExt;

#[derive(Debug)]
pub(super) struct Finding {
    mode: Mode,
    signature: String,
    detail: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "backend {}, MIR checks {}, JIT {}: {}\n{}",
            self.mode.0, self.mode.1, self.mode.2, self.signature, self.detail
        )
    }
}

pub(super) fn check(source: &str) -> Result<(), Finding> {
    let dir = tempfile::tempdir().expect("differential directory");
    let file = dir.path().join("generated.rock");
    std::fs::write(&file, source).expect("write differential program");
    let mut reference: Option<Vec<u8>> = None;
    for mode in [
        ("skull", false, "on"),
        ("js", false, "on"),
        ("skull", true, "off"),
        ("skull", false, "hot"),
        ("js", true, "on"),
    ] {
        let output = run(&file, mode).map_err(|(signature, detail)| Finding {
            mode,
            signature,
            detail,
        })?;
        if output.is_empty() {
            return Err(Finding {
                mode,
                signature: "empty stdout".into(),
                detail: "the generated program produced no trace".into(),
            });
        }
        if let Some(expected) = &reference {
            if expected != &output {
                let expected = String::from_utf8_lossy(expected);
                let actual = String::from_utf8_lossy(&output);
                let expected: Vec<_> = expected.split_inclusive('\n').collect();
                let actual: Vec<_> = actual.split_inclusive('\n').collect();
                let line = expected
                    .iter()
                    .zip(&actual)
                    .position(|(a, b)| a != b)
                    .unwrap_or_else(|| expected.len().min(actual.len()));
                return Err(Finding {
                    mode,
                    signature: "stdout mismatch".into(),
                    detail: format!(
                        "trace line {}: expected {:?}, actual {:?}",
                        line + 1,
                        expected.get(line),
                        actual.get(line)
                    ),
                });
            }
        } else {
            reference = Some(output);
        }
    }
    Ok(())
}

/// Backend, MIR checks, and LuaJIT compiler: default, off, or recording traces
/// at the first repetition.
type Mode = (&'static str, bool, &'static str);

fn run(source: &Path, mode: Mode) -> Result<Vec<u8>, (String, String)> {
    let io_error = |e: std::io::Error| (format!("process I/O: {:?}", e.kind()), e.to_string());
    let stdout = tempfile::tempfile().map_err(io_error)?;
    let stderr = tempfile::tempfile().map_err(io_error)?;
    let mut command = rock(&repo_root());
    command
        .args(["run", "--backend", mode.0])
        .arg(source)
        .env("ROCK_MIR_CHECKS", if mode.1 { "1" } else { "0" })
        .env("ROCK_DIFF_JIT", mode.2);
    match mode.2 {
        "off" => {
            command.env("LUA_INIT", "jit.off()");
        }
        "hot" => {
            command
                .env("ROCK_JIT_HOTLOOP", "1")
                .env("ROCK_JIT_HOTEXIT", "1");
        }
        _ => {}
    }
    command
        .stdout(Stdio::from(stdout.try_clone().map_err(io_error)?))
        .stderr(Stdio::from(stderr.try_clone().map_err(io_error)?));
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn().map_err(io_error)?;
    let status = child.wait_timeout(Duration::from_secs(15));
    if !matches!(status, Ok(Some(_))) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    let read = |mut file: std::fs::File| -> std::io::Result<Vec<u8>> {
        file.rewind()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    let output = read(stdout).map_err(io_error)?;
    let status = status.map_err(io_error)?;
    if status.is_some_and(|status| status.success()) {
        return Ok(output);
    }
    let errors = read(stderr).map_err(io_error)?;
    let detail = test_utils::strip_ansi_sgr(&format!(
        "{}{}",
        String::from_utf8_lossy(&output),
        String::from_utf8_lossy(&errors)
    ));
    match status {
        None => Err(("compiler or runtime timed out".into(), detail)),
        Some(status) if !status.success() => {
            let errors = test_utils::strip_ansi_sgr(&String::from_utf8_lossy(&errors));
            let message: &str = if errors.trim().is_empty() {
                &detail
            } else {
                &errors
            };
            Err((diagnostic(message, &status.to_string()), detail))
        }
        Some(_) => Ok(output),
    }
}

fn diagnostic(detail: &str, fallback: &str) -> String {
    let lines: Vec<_> = detail.lines().collect();
    let line = lines
        .iter()
        .enumerate()
        .find_map(|(i, line)| {
            if line.contains("panicked at") {
                Some(lines.get(i + 1).copied().unwrap_or(line))
            } else if line.to_ascii_lowercase().contains("error") {
                Some(*line)
            } else {
                None
            }
        })
        .or_else(|| lines.iter().copied().find(|line| !line.trim().is_empty()))
        .unwrap_or(fallback);
    line.split_whitespace()
        .map(|word| {
            if word.contains('/')
                || word.contains('\\')
                || [".rock:", ".rs:", ".lua:", ".js:"]
                    .iter()
                    .any(|suffix| word.contains(suffix))
            {
                "<location>"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn minimize(source: &str, finding: &Finding) -> String {
    let steps = std::env::var("ROCK_DIFF_SHRINK_STEPS").map_or(150, |s| {
        s.parse::<usize>()
            .expect("ROCK_DIFF_SHRINK_STEPS must be an integer")
    });
    let mut probes = 0;
    shrink(source, steps, |candidate| {
        probes += 1;
        if probes % 25 == 0 {
            eprintln!(
                "differential reduction: {probes}/{steps} probes, {} bytes",
                candidate.len()
            );
        }
        check(candidate)
            .is_err_and(|other| other.mode == finding.mode && other.signature == finding.signature)
    })
}

fn shrink(source: &str, steps: usize, mut interesting: impl FnMut(&str) -> bool) -> String {
    let mut best = source.to_owned();
    let mut attempts = 0;
    let deletion_budget = steps * 3 / 4;
    // Remove complete functions and compound statements before trying arbitrary cuts.
    while attempts < deletion_budget / 2 {
        let mut accepted = false;
        for range in balanced_ranges(&best) {
            if attempts >= deletion_budget / 2 {
                break;
            }
            if best[range.clone()].contains("jit_enabled") {
                continue;
            }
            let mut candidate = best.clone();
            candidate.replace_range(range, "");
            if !safe_loops(&candidate) {
                continue;
            }
            attempts += 1;
            if interesting(&candidate) {
                best = candidate;
                accepted = true;
                break;
            }
        }
        if !accepted {
            break;
        }
    }
    let mut width = (best.lines().count() / 2).max(1);
    while width > 0 && attempts < deletion_budget {
        let lines: Vec<_> = best.split_inclusive('\n').collect();
        let mut accepted = false;
        for start in (0..lines.len()).step_by(width) {
            if attempts >= deletion_budget {
                break;
            }
            let end = (start + width).min(lines.len());
            // Retain the assertion that the Lua target actually runs with its JIT on.
            if lines[start..end]
                .iter()
                .any(|line| line.contains("jit_enabled"))
            {
                continue;
            }
            let candidate = [&lines[..start], &lines[end..]].concat().concat();
            if brace_boundaries(&candidate).is_none() || !safe_loops(&candidate) {
                continue;
            }
            attempts += 1;
            if interesting(&candidate) {
                best = candidate;
                accepted = true;
                break;
            }
        }
        if !accepted {
            width /= 2;
        }
    }
    while attempts < steps {
        let mut accepted = false;
        for (range, replacement) in expressions(&best) {
            if attempts >= steps {
                break;
            }
            if replacement.len() >= range.len() {
                continue;
            }
            let mut candidate = best.clone();
            candidate.replace_range(range, &replacement);
            if !safe_loops(&candidate) {
                continue;
            }
            attempts += 1;
            if interesting(&candidate) {
                best = candidate;
                accepted = true;
                break;
            }
        }
        if !accepted {
            break;
        }
    }
    best
}

fn safe_loops(source: &str) -> bool {
    source.lines().all(|line| {
        let mut words = line.split_whitespace();
        if words.next() != Some("while") {
            return true;
        }
        let Some(name) = words.next() else {
            return true;
        };
        if !name
            .strip_prefix('v')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            || words.next() != Some("<")
        {
            return true;
        }
        source
            .lines()
            .any(|line| line.trim() == format!("{name} += 1"))
    })
}

fn brace_boundaries(source: &str) -> Option<Vec<(usize, usize)>> {
    let mut boundaries = vec![(0, 0)];
    let (mut offset, mut depth, mut quoted, mut escaped) = (0, 0usize, false, false);
    for line in source.split_inclusive('\n') {
        let bytes = line.as_bytes();
        for (i, &byte) in bytes.iter().enumerate() {
            if escaped {
                escaped = false;
                continue;
            }
            if quoted && byte == b'\\' {
                escaped = true;
                continue;
            }
            if byte == b'"' {
                quoted = !quoted;
            }
            if quoted {
                continue;
            }
            if byte == b'/' && bytes.get(i + 1) == Some(&b'/') {
                break;
            }
            if byte == b'{' {
                depth += 1;
            }
            if byte == b'}' {
                depth = depth.checked_sub(1)?;
            }
        }
        offset += line.len();
        boundaries.push((offset, depth));
    }
    (depth == 0 && !quoted).then_some(boundaries)
}

fn balanced_ranges(source: &str) -> Vec<Range<usize>> {
    let Some(boundaries) = brace_boundaries(source) else {
        return Vec::new();
    };
    let mut ranges = Vec::new();
    for (i, &(start, depth)) in boundaries.iter().enumerate() {
        for &(end, end_depth) in &boundaries[i + 1..] {
            if end_depth < depth {
                break;
            }
            if end_depth == depth {
                ranges.push(start..end);
                break;
            }
        }
    }
    ranges.sort_by_key(|range| std::cmp::Reverse(range.len()));
    ranges
}

fn expressions(source: &str) -> Vec<(Range<usize>, String)> {
    let mut candidates = Vec::new();
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        if !line.contains("jit_enabled") {
            let mut stack = Vec::new();
            let mut quoted = false;
            let mut escaped = false;
            for (i, byte) in line.bytes().enumerate() {
                if escaped {
                    escaped = false;
                    continue;
                }
                if byte == b'\\' && quoted {
                    escaped = true;
                    continue;
                }
                if byte == b'"' {
                    quoted = !quoted;
                }
                if quoted {
                    continue;
                }
                if byte == b'(' {
                    stack.push(i);
                }
                if byte == b')'
                    && let Some(start) = stack.pop()
                {
                    let body = &line[start + 1..i];
                    if body.trim().is_empty() {
                        continue;
                    }
                    let mut replacements = vec!["0", "1", "true", "false"];
                    if let Some((left, right)) = body
                        .split_once(" + ")
                        .or_else(|| body.split_once(" * "))
                        .or_else(|| body.split_once(" - "))
                    {
                        replacements.extend([left.trim(), right.trim()]);
                    }
                    for replacement in replacements {
                        candidates.push((offset + start..offset + i + 1, replacement.to_owned()));
                    }
                }
            }
        }
        offset += line.len();
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduction_keeps_the_trigger_and_its_dependency() {
        let source = "unused\nlet dependency = 7\nnoise\ntrigger((dependency + 0))\nmore noise\n";
        let reduced = shrink(source, 150, |s| {
            s.contains("let dependency = 7\n") && s.contains("trigger((dependency + 0))")
        });
        assert_eq!(reduced, "let dependency = 7\ntrigger((dependency + 0))\n");
    }

    #[test]
    fn reduction_simplifies_expressions_and_respects_budget() {
        let mut attempts = 0;
        let reduced = shrink("trigger((7 + 0))\n", 30, |s| {
            attempts += 1;
            s.contains("trigger(") && s.contains('7')
        });
        assert_eq!(reduced, "trigger(7)\n");
        assert!(attempts <= 30);
    }

    #[test]
    fn reduction_retains_the_jit_guard() {
        assert_eq!(
            shrink("assert(jit_enabled())\nnoise\n", 30, |_| true),
            "assert(jit_enabled())\n"
        );
    }

    #[test]
    fn reduction_keeps_live_loop_guards_but_can_remove_whole_loops() {
        let source = "let v12 = 0\nwhile v12 < 3 {\nv12 += 1\ntrigger()\n}\n";
        assert!(!safe_loops("let v12 = 0\nwhile v12 < 3 {\ntrigger()\n}\n"));
        assert!(safe_loops("let v12 = 0\n"));
        let live = shrink(source, 60, |candidate| {
            assert!(safe_loops(candidate), "unsafe candidate reached the runner");
            candidate.contains("while v12 < 3") && candidate.contains("trigger()")
        });
        assert!(live.contains("v12 += 1"));
        let removed = shrink(source, 60, |_| true);
        assert!(!removed.contains("while"));
    }

    #[test]
    fn reduction_removes_multiline_declarations_within_the_probe_budget() {
        let mut source = String::new();
        for n in 0..30 {
            source.push_str(&format!(
                "fn unused{n}() {{\nlet x = 0\nif true {{\nprint(x)\n}}\nprint(0)\nprint(1)\n}}\n"
            ));
        }
        source.push_str("// a\n// b\n// c\n// d\n// e\nfn main() {\nassert(jit_enabled())\nlet dependency = 7\ntrigger((dependency + 0))\n}\n");
        assert_eq!(source.lines().count(), 250);
        let mut probes = 0;
        let reduced = shrink(&source, 180, |candidate| {
            probes += 1;
            brace_boundaries(candidate).is_some()
                && candidate.contains("fn main() {")
                && candidate.contains("let dependency = 7")
                && candidate.contains("trigger((dependency + 0))")
        });
        assert!(reduced.lines().count() <= 6, "{reduced}");
        assert!(probes <= 180);
    }

    #[test]
    fn failure_signature_ignores_paths_but_keeps_the_error() {
        assert_eq!(
            diagnostic(
                "thread 'main' panicked at /tmp/one.rs:12:4:\nfailed invariant\n",
                "exit"
            ),
            "failed invariant"
        );
        assert_eq!(
            diagnostic("error: undefined name\n  --> /tmp/one.rock:4:2\n", "exit"),
            "error: undefined name"
        );
        assert_ne!(
            diagnostic("error: undefined name", "exit"),
            diagnostic("error: mismatched type", "exit")
        );
        assert_eq!(
            diagnostic("error: value 42 at generated.rock:12:4", "exit"),
            "error: value 42 at <location>"
        );
    }
}
