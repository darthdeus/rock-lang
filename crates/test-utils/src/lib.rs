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

use std::collections::BTreeMap;

/// Parse `//^^^ msg` annotations out of source text.
///
/// Returns `(byte_range, content)` pairs. `byte_range` points at characters
/// on the line *above* the annotation; content is the trimmed text after the
/// carets. Annotation lines themselves are not included in any range.
pub fn extract_annotations(text: &str) -> Vec<((usize, usize), String)> {
    let mut res: Vec<((usize, usize), String)> = Vec::new();
    // Map: line length → byte offset of the most recent line with that length.
    // Used to resolve which earlier line an annotation points at, since the
    // annotation line itself may be shorter or longer than the target.
    let mut line_start_map: BTreeMap<usize, usize> = BTreeMap::new();
    let mut line_start: usize = 0;
    let mut prev_line_annotations: Vec<(usize, usize)> = Vec::new();

    for line in text.split_inclusive('\n') {
        let mut this_line_annotations: Vec<(usize, usize)> = Vec::new();
        let split = line
            .split_once("//")
            .filter(|(_, suffix)| !suffix.trim_start().starts_with("//"));
        let line_length = if let Some((prefix, suffix)) = split {
            let annotation_offset = prefix.len() + "//".len();
            for ann in extract_line_annotations(suffix.trim_end_matches('\n')) {
                match ann {
                    LineAnnotation::Annotation { range, content } => {
                        let abs = (range.0 + annotation_offset, range.1 + annotation_offset);
                        this_line_annotations.push((abs.1, res.len()));
                        let (_, &target_line_start) = line_start_map
                            .range(abs.1..)
                            .next()
                            .expect("annotation refers to a line not yet seen");
                        let abs = (abs.0 + target_line_start, abs.1 + target_line_start);
                        res.push((abs, content));
                    }
                    LineAnnotation::Continuation { offset, content } => {
                        let offset = offset + annotation_offset;
                        let &(_, idx) = prev_line_annotations
                            .iter()
                            .find(|&&(off, _)| off == offset)
                            .expect("continuation has no preceding annotation");
                        res[idx].1.push('\n');
                        res[idx].1.push_str(&content);
                    }
                }
            }
            annotation_offset
        } else {
            line.len()
        };

        line_start_map = line_start_map.split_off(&line_length);
        line_start_map.insert(line_length, line_start);
        line_start += line.len();
        prev_line_annotations = this_line_annotations;
    }

    res
}

enum LineAnnotation {
    Annotation { range: (usize, usize), content: String },
    Continuation { offset: usize, content: String },
}

fn extract_line_annotations(mut line: &str) -> Vec<LineAnnotation> {
    let mut res = Vec::new();
    let mut offset: usize = 0;
    let marker: fn(char) -> bool = if line.contains('^') { |c| c == '^' } else { |c| c == '|' };
    while let Some(idx) = line.find(marker) {
        offset += idx;
        line = &line[idx..];

        let mut len = line.chars().take_while(|&it| it == '^').count();
        let mut continuation = false;
        if len == 0 {
            assert!(line.starts_with('|'), "expected ^ or |");
            continuation = true;
            len = 1;
        }
        let range = (offset, offset + len);
        let after_caret = &line[len..];
        let end_marker = after_caret.find('$');
        let next = after_caret.find(marker).map_or(line.len(), |it| it + len);

        let cond = |end_marker: usize| {
            end_marker < next
                && (after_caret[end_marker + 1..].is_empty()
                    || after_caret[end_marker + 1..]
                        .strip_prefix(|c: char| c.is_whitespace() || c == '^')
                        .is_some())
        };
        let content = match end_marker {
            Some(em) if cond(em) => &after_caret[..em],
            _ => after_caret[..next - len].trim_end(),
        };

        let content = content.trim_start().to_owned();

        let annotation = if continuation {
            LineAnnotation::Continuation { offset: range.1, content }
        } else {
            LineAnnotation::Annotation { range, content }
        };
        res.push(annotation);

        line = &line[next..];
        offset += next;
    }

    res
}

/// A named section parsed from a fixture file via `//- name` markers.
#[derive(Debug)]
pub struct Section<'a> {
    pub name: &'a str,
    pub content: &'a str,
}

/// Parse `//- name` section markers out of a fixture file.
///
/// A line starting with `//- ` (column 0, space required) begins a new
/// section; everything up to the next marker (or EOF) is that section's
/// content. Any text before the first marker is ignored — fixtures with
/// no markers return an empty vec, and the caller should treat the whole
/// file as a single unnamed case.
pub fn parse_sections(src: &str) -> Vec<Section<'_>> {
    let mut out = Vec::new();
    let mut current: Option<(&str, usize)> = None;
    let mut cursor = 0;

    for line in src.split_inclusive('\n') {
        let line_end = cursor + line.len();
        if let Some(name) = line.trim_end().strip_prefix("//- ") {
            if let Some((prev_name, prev_start)) = current.take() {
                out.push(Section { name: prev_name, content: &src[prev_start..cursor] });
            }
            current = Some((name, line_end));
        }
        cursor = line_end;
    }
    if let Some((name, start)) = current {
        out.push(Section { name, content: &src[start..] });
    }
    out
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
        // Range points at the `x` in `let x`.
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
        // Source line must extend past the caret columns; `hello` (5 chars)
        // covers cols 0–4 so `^^^` at cols 2–4 points at "llo".
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
