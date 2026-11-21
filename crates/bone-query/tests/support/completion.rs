//! Completion fixtures: `<|>` is the cursor; every response must satisfy the response invariants.

use crate::fixture::position_of_offset;
pub use crate::label_asserts::{assert_offers, label_matches};
use bone_query::BoneWorkspace;
use lsp_types::{CompletionItem, InsertTextFormat, Position};

#[cfg(not(windows))]
pub const PATH: &str = "/tmp/rock_lsp_completion.rock";
#[cfg(windows)]
pub const PATH: &str = r"C:\tmp\rock_lsp_completion.rock";

const CURSOR: &str = "<|>";

/// `marked` without its cursor marker, and the marker's position (UTF-16
/// column, as LSP counts).
pub fn cursor(marked: &str) -> (String, Position) {
    let at = marked
        .find(CURSOR)
        .unwrap_or_else(|| panic!("no `<|>` cursor in {marked:?}"));
    (
        marked.replacen(CURSOR, "", 1),
        position_of_offset(marked, at),
    )
}

/// The response at the cursor, in rank order.
pub fn complete(marked: &str) -> Vec<CompletionItem> {
    complete_in(PATH, marked)
}

pub fn complete_in(path: &str, marked: &str) -> Vec<CompletionItem> {
    let (src, pos) = cursor(marked);
    let mut bone = BoneWorkspace::new();
    bone.update(path, src);
    let items = bone.completion(path, pos);
    assert_invariants(marked, &items);
    items
}

/// Checks the response invariants against the identifier prefix typed
/// before the cursor.
#[track_caller]
pub fn assert_invariants(marked: &str, items: &[CompletionItem]) {
    let head = &marked.as_bytes()[..marked.find(CURSOR).expect("cursor")];
    let start = head
        .iter()
        .rposition(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
        .map_or(0, |i| i + 1);
    let prefix = String::from_utf8_lossy(&head[start..]).into_owned();
    let violations = crate::response_invariants::completion_response_violations(items, &prefix);
    assert!(
        violations.is_empty(),
        "response invariants violated (prefix {prefix:?}) in {marked:?}: {violations:?}"
    );
}

pub fn labels_of(items: &[CompletionItem]) -> Vec<String> {
    items.iter().map(|c| c.label.clone()).collect()
}

pub fn labels(marked: &str) -> Vec<String> {
    labels_of(&complete(marked))
}

#[track_caller]
pub fn find<'a>(items: &'a [CompletionItem], label: &str) -> &'a CompletionItem {
    items
        .iter()
        .find(|c| c.label == label)
        .unwrap_or_else(|| panic!("no completion `{label}` in {:?}", labels_of(items)))
}

#[track_caller]
pub fn item(marked: &str, label: &str) -> CompletionItem {
    find(&complete(marked), label).clone()
}

/// Position of the first label matching `pattern`.
#[track_caller]
pub fn rank(labels: &[String], pattern: &str) -> usize {
    labels
        .iter()
        .position(|l| label_matches(l, pattern))
        .unwrap_or_else(|| panic!("no completion `{pattern}` in {labels:?}"))
}

/// `order` is one or more `; `-separated chains `a < b < c` of label
/// patterns; each must rank strictly above the next, by response position and
/// by sort text. A `?`-prefixed pattern may be absent.
#[track_caller]
pub fn assert_ranked(case: &str, items: &[CompletionItem], order: &str) {
    let labels = labels_of(items);
    for chain in order.split("; ") {
        let mut above: Option<(usize, &str)> = None;
        for pattern in chain.split(" < ") {
            let (pattern, optional) = match pattern.strip_prefix('?') {
                Some(p) => (p, true),
                None => (pattern, false),
            };
            let Some(at) = labels.iter().position(|l| label_matches(l, pattern)) else {
                assert!(optional, "{case}: `{pattern}` not offered: {labels:?}");
                continue;
            };
            if let Some((prev, prev_pattern)) = above {
                assert!(
                    prev < at && items[prev].sort_text < items[at].sort_text,
                    "{case}: `{prev_pattern}` must rank above `{pattern}`: {labels:?}"
                );
            }
            above = Some((at, pattern));
        }
    }
}

/// How accepting an item edits the buffer.
#[derive(Debug, Clone, Copy)]
pub enum Insert {
    /// This insert text, in any format.
    Text(&'static str),
    /// This insert text as a snippet.
    Snippet(&'static str),
    /// This insert text as plain text: the cursor lands after it.
    Plain(&'static str),
    /// No insert text: the label itself is inserted.
    Label,
}

#[track_caller]
pub fn assert_insert(case: &str, it: &CompletionItem, want: Insert) {
    let (text, format) = match want {
        Insert::Text(t) => (Some(t), it.insert_text_format),
        Insert::Snippet(t) => (Some(t), Some(InsertTextFormat::SNIPPET)),
        Insert::Plain(t) => (Some(t), None),
        Insert::Label => (None, None),
    };
    assert_eq!(
        (it.insert_text.as_deref(), it.insert_text_format),
        (text, format),
        "{case}: {it:?}"
    );
}

/// Each pattern must match an offered label; a `!`-prefixed one must match
/// none.
#[track_caller]
pub fn assert_labels(case: &str, labels: &[String], patterns: &[&str]) {
    for pattern in patterns {
        match pattern.strip_prefix('!') {
            Some(p) => assert!(
                !labels.iter().any(|l| label_matches(l, p)),
                "{case}: `{p}` must not be offered: {labels:?}"
            ),
            None => assert!(
                labels.iter().any(|l| label_matches(l, pattern)),
                "{case}: `{pattern}` not offered: {labels:?}"
            ),
        }
    }
}

/// Rows of (case, fixture, label patterns for `assert_labels`).
pub fn check_labels(cases: &[(&str, &str, &[&str])]) {
    for (case, src, patterns) in cases {
        assert_labels(case, &labels(src), patterns);
    }
}

/// Rows of (case, fixture): the response is empty.
pub fn check_empty(cases: &[(&str, &str)]) {
    for (case, src) in cases {
        assert_eq!(labels(src), Vec::<String>::new(), "{case}");
    }
}

/// Rows of (case, fixture, order for `assert_ranked`).
pub fn check_ranked(cases: &[(&str, &str, &str)]) {
    for (case, src, order) in cases {
        assert_ranked(case, &complete(src), order);
    }
}

#[track_caller]
pub fn assert_head(case: &str, labels: &[String], head: &[&str]) {
    assert_eq!(
        &labels[..head.len().min(labels.len())],
        head,
        "{case}: {labels:?}"
    );
}

/// Rows of (case, fixture, the labels that open the response, in order).
pub fn check_head(cases: &[(&str, &str, &[&str])]) {
    for (case, src, head) in cases {
        assert_head(case, &labels(src), head);
    }
}

/// Rows of (case, fixture, label, how accepting it edits the buffer).
pub fn check_inserts(cases: &[(&str, &str, &str, Insert)]) {
    for (case, src, label, want) in cases {
        assert_insert(case, &item(src, label), *want);
    }
}
