//! Source trivia the parser drops: trailing same-line comments, standalone
//! (own-line) line comments, and blank-line positions. Standalone comments at
//! top level / inside blocks survive in the AST as `Comment` items / statements;
//! those inside delimited bodies (struct/enum fields) are dropped by the parser
//! and recovered here by source line.

use rock_tokenizer::{TokenKind, Tokenizer};
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
    /// its text without the leading `//`.
    standalone: BTreeMap<usize, String>,
    /// Whether each 0-based line is blank (empty or whitespace-only).
    blank: Vec<bool>,
    /// Block comments (`/* ... */`) keyed by start byte offset: the FULL source
    /// slice (delimiters included) and the 0-based line it starts on. Collected
    /// separately from `//` -- a block comment can sit inline anywhere and MUST
    /// round-trip verbatim in place (never converted to a `//` line comment, and
    /// never dropped). The formatter treats a construct that carries one as a
    /// "preserve verbatim" trigger.
    block: BTreeMap<usize, BlockComment>,
    /// Tokens other than comments, commas and newlines, by start offset; a `)`
    /// carries its `(end offset, 0-based line)`.
    code: BTreeMap<usize, Option<(usize, usize)>>,
    /// Start offsets of every `//` comment, trailing or standalone.
    line_comments: BTreeSet<usize>,
}

struct BlockComment {
    /// Full source slice including `/*` and `*/`.
    text: String,
    /// 0-based line the comment starts on.
    line: usize,
    /// The comment is alone on its source line (line opens with `/*`).
    own_line: bool,
}

impl Trivia {
    pub fn harvest(src: &str, file: Ustr) -> Result<Trivia, String> {
        let tokens = Tokenizer::new(src, file)
            .tokenize()
            .map_err(|e| format!("{e:?}"))?;

        let lines = src.lines().collect::<Vec<_>>();

        let mut trailing = HashMap::new();
        let mut standalone = BTreeMap::new();
        let mut block = BTreeMap::new();
        let mut code = BTreeMap::new();
        let mut line_comments = BTreeSet::new();
        // Track the last line on which we saw a non-comment token, so a comment
        // sharing that line is classified as trailing.
        let mut code_on_line: Option<usize> = None;
        for tok in &tokens {
            let line = tok.span.line_range.0;
            if !matches!(
                tok.kind,
                TokenKind::Comment(_) | TokenKind::Comma | TokenKind::Newline
            ) {
                let close = (tok.kind == TokenKind::CloseParen)
                    .then_some((tok.span.offset_range.1, tok.span.line_range.1));
                code.insert(tok.span.offset_range.0, close);
            }
            match &tok.kind {
                TokenKind::Comment(text) => {
                    let (lo, hi) = tok.span.offset_range;
                    // A `/* */` block comment shares the `Comment` kind but must
                    // NOT enter the `//` trailing/standalone maps -- those render
                    // as `// text`, silently rewriting `/* text */` and dropping
                    // its in-place position. Record it verbatim, by offset.
                    if src.as_bytes().get(lo) == Some(&b'/')
                        && src.as_bytes().get(lo + 1) == Some(&b'*')
                    {
                        // Own-line = the comment IS the whole line's content, not
                        // merely that the line opens with `/*` -- a line like
                        // `/* a */ x /* b */` opens with `/*` but neither comment
                        // is own-line (the trailing one must stay inline).
                        let full = source_slice(src, lo, hi);
                        let own_line = lines.get(line).is_some_and(|l| l.trim() == full);
                        block.insert(
                            lo,
                            BlockComment {
                                text: full.to_string(),
                                line,
                                own_line,
                            },
                        );
                    } else if code_on_line == Some(line) {
                        line_comments.insert(lo);
                        trailing
                            .insert(line, (text.as_str().to_string(), tok.span.col_range.0, lo));
                    } else if lines
                        .get(line)
                        .is_some_and(|l| l.trim_start().starts_with("//"))
                    {
                        line_comments.insert(lo);
                        standalone.insert(line, text.as_str().to_string());
                    }
                }
                _ => code_on_line = Some(line),
            }
        }

        let blank = lines
            .iter()
            .map(|l| l.trim().is_empty())
            .collect::<Vec<_>>();

        Ok(Trivia {
            trailing,
            standalone,
            blank,
            block,
            code,
            line_comments,
        })
    }

    /// The trailing comment on a given 0-based line: `(text without //, source
    /// column of the //, start byte offset of the //)`.
    pub fn trailing_comment(&self, line: usize) -> Option<(&str, usize, usize)> {
        self.trailing
            .get(&line)
            .map(|(t, c, o)| (t.as_str(), *c, *o))
    }

    /// The `)` that closes an argument list whose last argument ends at
    /// `offset`: `(end offset, 0-based line)`. `None` when other code comes
    /// first, as after the argument of a `f $ x` call.
    pub fn close_paren_after(&self, offset: usize) -> Option<(usize, usize)> {
        let (_, close) = self
            .code
            .range((Bound::Included(offset), Bound::Unbounded))
            .next()?;
        *close
    }

    /// Standalone `//` comments on lines strictly between `after` and `before`
    /// (both 0-based, both exclusive), in source order: `(line, text-without-//)`.
    /// Used to recover comments inside delimited bodies the parser discards.
    pub fn standalone_comments_between(&self, after: usize, before: usize) -> Vec<(usize, &str)> {
        if after + 1 >= before {
            return Vec::new();
        }
        self.standalone
            .range((Bound::Excluded(after), Bound::Excluded(before)))
            .map(|(l, t)| (*l, t.as_str()))
            .collect()
    }

    /// The nearest inline (not own-line) block comment whose START is at or
    /// after `offset`: `(start, end)`. The caller checks the gap between
    /// `offset` and `start` is whitespace/`,` before treating it as a trailing
    /// comment on the node ending at `offset` (`3 /* end */`).
    pub fn inline_block_starting_after(&self, offset: usize) -> Option<(usize, usize)> {
        self.block
            .range((Bound::Included(offset), Bound::Unbounded))
            .find(|(_, b)| !b.own_line)
            .map(|(start, b)| (*start, *start + b.text.len()))
    }

    /// True if any block comment starts within `[lo, hi)`. The "preserve
    /// verbatim" trigger: a construct whose span carries a block comment is
    /// sliced from source rather than re-rendered (which would drop it).
    pub fn has_block_comment_in(&self, lo: usize, hi: usize) -> bool {
        self.block
            .range((Bound::Included(lo), Bound::Excluded(hi)))
            .next()
            .is_some()
    }

    /// True if a block comment sharing its line with code or another comment
    /// starts within `[lo, hi)`.
    pub fn has_inline_block_comment_in(&self, lo: usize, hi: usize) -> bool {
        self.block
            .range((Bound::Included(lo), Bound::Excluded(hi)))
            .any(|(_, b)| !b.own_line)
    }

    /// True if a `//` comment starts within `[lo, hi)`.
    pub fn has_line_comment_in(&self, lo: usize, hi: usize) -> bool {
        self.line_comments
            .range((Bound::Included(lo), Bound::Excluded(hi)))
            .next()
            .is_some()
    }

    /// True if an own-line comment of either form sits on a line strictly
    /// between `after` and `before` (0-based, exclusive).
    pub fn has_own_line_comment_between(&self, after: usize, before: usize) -> bool {
        !self.standalone_comments_between(after, before).is_empty()
            || !self
                .own_line_block_comments_between(after, before)
                .is_empty()
    }

    /// The block comment (start, end) immediately preceding `offset` -- i.e. the
    /// nearest one whose END is at or before `offset`. The caller checks the gap
    /// between `end` and `offset` is whitespace before treating it as a leading
    /// comment on the node at `offset`.
    pub fn block_comment_ending_before(&self, offset: usize) -> Option<(usize, usize)> {
        self.block
            .range((Bound::Unbounded, Bound::Included(offset)))
            .rev()
            .map(|(start, b)| (*start, *start + b.text.len()))
            .find(|(_, end)| *end <= offset)
    }

    /// Own-line block comments (`/* ... */` alone on their line) starting on a
    /// line strictly between `after` and `before` (0-based, exclusive), in
    /// source order: `(start_offset, end_offset, start_line)`. Recovered like
    /// `//` standalone comments in bodies the parser discards.
    pub fn own_line_block_comments_between(
        &self,
        after: usize,
        before: usize,
    ) -> Vec<(usize, usize, usize)> {
        if after + 1 >= before {
            return Vec::new();
        }
        self.block
            .iter()
            .filter(|(_, b)| b.own_line && b.line > after && b.line < before)
            .map(|(start, b)| (*start, *start + b.text.len(), b.line))
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

/// Tokenizer and parser spans must address the original source exactly.
pub(crate) fn source_slice(src: &str, lo: usize, hi: usize) -> &str {
    src.get(lo..hi).expect("invalid source span in formatter")
}
