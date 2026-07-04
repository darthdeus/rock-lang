//! Source trivia the parser drops: trailing same-line comments, standalone
//! (own-line) line comments, and blank-line positions. Standalone comments at
//! top level / inside blocks survive in the AST as `Comment` items / statements;
//! those inside delimited bodies (struct/enum fields) are dropped by the parser
//! and recovered here by source line.

use rock_tokenizer::{TokenKind, Tokenizer};
use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use ustr::Ustr;

pub struct Trivia {
    /// Per 0-based source line: a trailing comment that follows other code on
    /// the same line -- its text (without leading `//`), the 0-based column the
    /// `//` started at in source (used to preserve the author's alignment
    /// column), and the comment's start byte offset (used to check what code
    /// actually precedes it, so only the construct it trails claims it).
    trailing: HashMap<usize, (String, usize, usize)>,
    /// Per 0-based source line that holds ONLY a `//` line comment (no code):
    /// its text without the leading `//`. Block comments (`/* */`) are not
    /// collected -- they round-trip verbatim elsewhere.
    standalone: BTreeMap<usize, String>,
    /// Whether each 0-based line is blank (empty or whitespace-only).
    blank: Vec<bool>,
}

impl Trivia {
    pub fn harvest(src: &str, file: Ustr) -> Result<Trivia, String> {
        let tokens = Tokenizer::new(src, file)
            .tokenize()
            .map_err(|e| format!("{e:?}"))?;

        let lines = src.lines().collect::<Vec<_>>();

        let mut trailing = HashMap::new();
        let mut standalone = BTreeMap::new();
        // Track the last line on which we saw a non-comment token, so a comment
        // sharing that line is classified as trailing.
        let mut code_on_line: Option<usize> = None;
        for tok in &tokens {
            let line = tok.span.line_range.0;
            match &tok.kind {
                TokenKind::Comment(text) => {
                    if code_on_line == Some(line) {
                        trailing.insert(
                            line,
                            (
                                text.as_str().to_string(),
                                tok.span.col_range.0,
                                tok.span.offset_range.0,
                            ),
                        );
                    } else if lines.get(line).is_some_and(|l| l.trim_start().starts_with("//")) {
                        // Own-line `//` comment. `/* */` block comments share the
                        // `Comment` kind but slice differently, so gate on the
                        // source line opening with `//`.
                        standalone.insert(line, text.as_str().to_string());
                    }
                }
                _ => code_on_line = Some(line),
            }
        }

        let blank = lines.iter().map(|l| l.trim().is_empty()).collect::<Vec<_>>();

        Ok(Trivia {
            trailing,
            standalone,
            blank,
        })
    }

    /// The trailing comment on a given 0-based line: `(text without //, source
    /// column of the //, start byte offset of the //)`.
    pub fn trailing_comment(&self, line: usize) -> Option<(&str, usize, usize)> {
        self.trailing.get(&line).map(|(t, c, o)| (t.as_str(), *c, *o))
    }

    /// Standalone `//` comments on lines strictly between `after` and `before`
    /// (both 0-based, both exclusive), in source order: `(line, text-without-//)`.
    /// Used to recover comments inside delimited bodies the parser discards.
    pub fn standalone_comments_between(
        &self,
        after: usize,
        before: usize,
    ) -> Vec<(usize, &str)> {
        if after + 1 >= before {
            return Vec::new();
        }
        self.standalone
            .range((Bound::Excluded(after), Bound::Excluded(before)))
            .map(|(l, t)| (*l, t.as_str()))
            .collect()
    }

    /// Number of fully blank lines strictly between two 0-based line numbers,
    /// capped at 2 (the formatter never emits more than two consecutive blanks).
    pub fn blank_count_between(&self, end_line: usize, start_line: usize) -> usize {
        if start_line <= end_line + 1 {
            return 0;
        }
        let n = ((end_line + 1)..start_line)
            .filter(|l| self.blank.get(*l).copied().unwrap_or(false))
            .count();
        n.min(2)
    }
}
