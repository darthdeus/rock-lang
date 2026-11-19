//! Test helpers shared across bone (and friends).
//!
//! `extract_annotations` is a port of rust-analyzer's helper of the same
//! name (`crates/test-utils/src/lib.rs` in rust-analyzer), adapted to plain
//! `usize` byte offsets instead of rowan's `TextRange`/`TextSize`. The
//! single-file restriction (no `^file` markers) keeps the surface small.
//!
//! rust-analyzer (https://github.com/rust-lang/rust-analyzer) is licensed
//! under MIT OR Apache-2.0; the port is used under its MIT license. That
//! license's text is the same as this repository's `LICENSE-MIT` without the
//! copyright line, and requires its notice to stay with copies of the code.
//!
//! Annotation syntax — anchored at the column of the leading `^`, pointing
//! at the line above:
//!
//! ```text
//! let x = 5;
//!   //^ int
//! ```
//!
//! Multiple annotations per line:
//!
//! ```text
//! let (x,     y) = (9, 2);
//!    //^ int  ^ int
//! ```
//!
//! Continuation lines (multi-line content) via `|`:
//!
//! ```text
//! zoo + 1
//! //^^^^ type:
//! //   | int
//! ```

use std::time::Duration;

/// Hang backstop for a test that waits on an event, a condition or a child.
/// A test fails only for a defect or a hang, never because its host is busy:
/// the wait ends on the event, and this bound only catches a hang on a loaded
/// host. It sits inside nextest's 15-minute kill for the rock-cli binaries
/// and the full profile.
pub const HANG_BACKSTOP: Duration = Duration::from_secs(300);

pub fn extract_annotations(text: &str) -> Vec<((usize, usize), String)> {
    let mut res: Vec<((usize, usize), String)> = Vec::new();
    let mut targets = Vec::new();
    let mut line_start = 0;
    let mut previous_annotations = Vec::<(usize, usize)>::new();
    let mut line_annotations = Vec::new();

    for line in text.split_inclusive('\n') {
        line_annotations.clear();
        let split = line
            .split_once("//")
            .filter(|(_, suffix)| !suffix.trim_start().starts_with("//"));
        let line_length = if let Some((prefix, suffix)) = split {
            let annotation_offset = prefix.len() + "//".len();
            let suffix = suffix.trim_end_matches('\n');
            let marker = if suffix.contains('^') { '^' } else { '|' };
            for ((start, end), content) in annotations_on_line(suffix, marker) {
                let end = end + annotation_offset;
                if marker == '|' {
                    let &(_, idx) = previous_annotations
                        .iter()
                        .find(|&&(column, _)| column == end)
                        .expect("continuation has no preceding annotation");
                    res[idx].1.push('\n');
                    res[idx].1.push_str(content);
                } else {
                    let target = targets.partition_point(|&(length, _)| length >= end);
                    let &(_, target_start) = targets[..target]
                        .last()
                        .expect("annotation refers to a line not yet seen");
                    let range = (start + annotation_offset + target_start, end + target_start);
                    line_annotations.push((end, res.len()));
                    res.push((range, content.to_owned()));
                }
            }
            annotation_offset
        } else {
            line.len()
        };

        while targets
            .last()
            .is_some_and(|&(length, _)| length <= line_length)
        {
            targets.pop();
        }
        targets.push((line_length, line_start));
        line_start += line.len();
        std::mem::swap(&mut previous_annotations, &mut line_annotations);
    }

    res
}

fn annotations_on_line(
    mut line: &str,
    marker: char,
) -> impl Iterator<Item = ((usize, usize), &str)> {
    let mut offset = 0;
    std::iter::from_fn(move || {
        let idx = line.find(marker)?;
        let start = offset + idx;
        line = &line[idx..];
        let len = line.bytes().take_while(|&byte| byte == b'^').count().max(1);
        let rest = &line[len..];
        let next = rest.find(marker).map_or(line.len(), |idx| idx + len);
        let terminates = |end: usize| {
            end < next
                && rest[end + 1..]
                    .chars()
                    .next()
                    .is_none_or(|c| c.is_whitespace() || c == '^')
        };
        let content = match rest.find('$').filter(|&end| terminates(end)) {
            Some(end) => &rest[..end],
            None => rest[..next - len].trim_end(),
        };
        line = &line[next..];
        offset = start + next;
        Some(((start, start + len), content.trim_start()))
    })
}

#[derive(Debug)]
pub struct Section<'a> {
    pub name: &'a str,
    pub content: &'a str,
}

pub fn parse_sections(src: &str) -> Vec<Section<'_>> {
    fixture_sections(src, |line| line.trim_end().strip_prefix("//- "))
        .map(|(name, content)| Section { name, content })
        .collect()
}

fn fixture_sections<'a>(
    src: &'a str,
    parse_header: impl Fn(&'a str) -> Option<&'a str> + 'a,
) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
    let mut lines = src.split_inclusive('\n');
    let mut offset = 0;
    let mut current = None;
    std::iter::from_fn(move || {
        for line in lines.by_ref() {
            let start = offset;
            offset += line.len();
            let Some(header) = parse_header(line) else {
                continue;
            };
            if let Some((previous, body_start)) = current.replace((header, offset)) {
                return Some((previous, &src[body_start..start]));
            }
        }
        let (header, body_start) = current.take()?;
        Some((header, &src[body_start..]))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_annotation() {
        let text = "\
let x = 5;
  //^ int
";
        let res = extract_annotations(text);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].1, "int");
        let (s, e) = res[0].0;
        assert_eq!(&text[s..e], "x");
    }

    #[test]
    fn multiple_per_line() {
        let text = "\
let (x,     y) = (9, 2);
   //^ int  ^ int
";
        let res = extract_annotations(text);
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].1, "int");
        assert_eq!(res[1].1, "int");
        let (s0, _) = res[0].0;
        let (s1, _) = res[1].0;
        assert_eq!(&text[s0..s0 + 1], "x");
        assert_eq!(&text[s1..s1 + 1], "y");
    }

    #[test]
    fn sections_basic() {
        let src = "\
//- first
hello
//- second
world more
";
        let s = parse_sections(src);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].name, "first");
        assert_eq!(s[0].content, "hello\n");
        assert_eq!(s[1].name, "second");
        assert_eq!(s[1].content, "world more\n");
    }

    #[test]
    fn sections_none_returns_empty() {
        let src = "fn f() {}\n";
        assert!(parse_sections(src).is_empty());
    }

    #[test]
    fn embedded_files_borrow_lf_and_normalize_crlf() {
        let body = concat!(
            "ignored\r\n",
            "  //-- app.rock @pkg=old,7 @v1 @pkg=app,9\r\n",
            "let x = 1;\r\n",
            "//^ int\r\n",
            "//--\n",
            "last\n\n",
        );
        let files = parse_embedded_files(body);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].file, "app.rock");
        assert_eq!(files[0].package, Some(("app", 9)));
        assert_eq!(files[0].src, "let x = 1;\n//^ int");
        assert_eq!(files[1].file, "file.rock");
        assert_eq!(files[1].package, None);
        assert_eq!(files[1].src, "last\n");
        let (_, lf_source) = body.split_once("//--\n").unwrap();
        assert_eq!(files[1].src.as_ptr(), lf_source.as_ptr());
    }

    #[test]
    fn doubly_commented_lines_ignored() {
        let text = "\
fn f() -> int { 5 }
        //^ int
// fn f() -> int { 5 }
//        // ^ int
";
        let res = extract_annotations(text);
        assert_eq!(res.len(), 1, "only the live annotation should be picked up");
        assert_eq!(res[0].1, "int");
    }

    #[test]
    fn continuation() {
        let text = "\
hello
//^^^ first
//  | second
";
        let res = extract_annotations(text);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].1, "first\nsecond");
    }
}

pub fn strip_ansi_sgr(text: &str) -> String {
    static ANSI_SGR: std::sync::LazyLock<regex_lite::Regex> = std::sync::LazyLock::new(|| {
        regex_lite::Regex::new(r"\x1b\[[0-9;]*m").expect("valid ANSI SGR regex")
    });
    ANSI_SGR.replace_all(text, "").into_owned()
}

/// Create an executable script for a test to spawn, replacing any file at
/// `path`.
///
/// On Linux, exec fails with ETXTBSY while any process holds the file open
/// for writing. Test binaries spawn from many threads, and a child forked by
/// another thread inherits every descriptor open at that moment and keeps it
/// until it reaches exec; O_CLOEXEC closes it no earlier. A script written
/// with `std::fs::write` in the test process can therefore still be open for
/// writing in an unrelated child when the test runs it. Here the write
/// happens in a short-lived `/bin/sh` that has exited before this returns, so
/// the test process never holds a write descriptor that a fork could copy.
/// The rename replaces an existing file instead of truncating it in place.
#[cfg(unix)]
pub fn write_executable(path: &std::path::Path, contents: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("/bin/sh")
        .args([
            "-c",
            r#"partial="$1.partial.$$" && cat > "$partial" && chmod 755 "$partial" && mv -f "$partial" "$1""#,
            "sh",
        ])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn /bin/sh to write a test executable");
    let mut stdin = child.stdin.take().expect("piped stdin");
    stdin
        .write_all(contents.as_bytes())
        .expect("write test executable contents");
    drop(stdin);
    let status = child.wait().expect("wait for /bin/sh");
    assert!(
        status.success(),
        "writing test executable {} failed: {status}",
        path.display()
    );
}

#[cfg(test)]
mod ansi_tests {
    use super::strip_ansi_sgr;

    #[test]
    fn strips_sgr_without_changing_unicode_or_layout() {
        let rendered = "\x1b[31m\u{e9}\n   \x1b[38;5;246m\u{2502}\x1b[0m\n";
        assert_eq!(strip_ansi_sgr(rendered), "\u{e9}\n   \u{2502}\n");
    }

    #[test]
    fn leaves_plain_text_unchanged() {
        let rendered = "[E00] Error: plain text\n 1 \u{2502} let x = 1;\n";
        assert_eq!(strip_ansi_sgr(rendered), rendered);
    }
}

#[cfg(all(test, unix))]
mod executable_tests {
    use super::write_executable;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn written_executable_runs_and_replaces_an_existing_one() {
        let dir = std::env::temp_dir().join(format!("test-utils-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("probe");
        for code in [3, 99] {
            let script = format!("#!/bin/sh\necho 'it''s ok'\nexit {code}\n");
            write_executable(&path, &script);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), script);
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
            let output = std::process::Command::new(&path).output().unwrap();
            assert_eq!(output.status.code(), Some(code));
            assert_eq!(output.stdout, b"its ok\n");
        }
        let entries = std::fs::read_dir(&dir).unwrap().count();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(entries, 1, "partial files left behind");
    }
}

/// One embedded file inside a fixture section: `//-- <file> @pkg=<name>,<id> @v1`.
pub struct EmbeddedFile<'a> {
    pub file: &'a str,
    pub package: Option<(&'a str, u32)>,
    pub src: &'a str,
}

/// Split a fixture section body into embedded files. Lines `//-- <name>
/// [@pkg=<self_name>,<id>] [@v1]` start a file; everything until the next
/// marker is that file's source. Embedded source normalizes CRLF to LF
/// and omits the final line ending.
pub fn parse_embedded_files(body: &str) -> Vec<EmbeddedFile<'_>> {
    fixture_sections(body, |line| line.trim_start().strip_prefix("//--"))
        .map(|(header, src)| {
            let mut tokens = header.split_whitespace();
            let file = tokens.next().unwrap_or("file.rock");
            let package = tokens
                .rev()
                .find_map(|token| token.strip_prefix("@pkg="))
                .map(|spec| {
                    let mut parts = spec.split(',');
                    let name = parts.next().unwrap_or_default();
                    let id = parts.next().and_then(|id| id.parse().ok()).unwrap_or(0);
                    (name, id)
                });
            let src = src
                .strip_suffix("\r\n")
                .or_else(|| src.strip_suffix('\n'))
                .unwrap_or(src);
            let src = if src.contains("\r\n") {
                Box::leak(src.replace("\r\n", "\n").into_boxed_str())
            } else {
                src
            };
            EmbeddedFile { file, package, src }
        })
        .collect()
}
