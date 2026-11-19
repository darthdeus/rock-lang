use rock_core::Span;
use rock_tokenizer::{TokenKind, Tokenizer};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Bound;
use ustr::Ustr;

pub(crate) struct Trivia {
    trailing: HashMap<usize, (String, usize, usize)>,
    standalone: BTreeMap<usize, String>,
    blank: Vec<bool>,
    /// Block comments (`/* ... */`) keyed by start byte offset: the FULL source
    /// slice (delimiters included) and the 0-based line it starts on. Collected
    /// separately from `//` -- a block comment can sit inline anywhere and MUST
    /// round-trip verbatim in place (never converted to a `//` line comment, and
    /// never dropped). The formatter treats a construct that carries one as a
    /// "preserve verbatim" trigger.
    block: BTreeMap<usize, BlockComment>,
    code: BTreeMap<usize, Option<(usize, usize)>>,
    line_comments: BTreeSet<usize>,
}

struct BlockComment {
    end: usize,
    line: usize,
    own_line: bool,
}

impl Trivia {
    pub(crate) fn harvest(src: &str, file: Ustr) -> Result<Trivia, String> {
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
            let line = tok.span.line_range().0;
            if !matches!(
                tok.kind,
                TokenKind::Comment(_) | TokenKind::Comma | TokenKind::Newline
            ) {
                let close = (tok.kind == TokenKind::CloseParen)
                    .then_some((tok.span.offset_range().1, tok.span.line_range().1));
                code.insert(tok.span.offset_range().0, close);
            }
            match &tok.kind {
                TokenKind::Comment(text) => {
                    let (lo, hi) = tok.span.offset_range();
                    // A `/* */` block comment shares the `Comment` kind but must
                    // NOT enter the `//` trailing/standalone maps -- those render
                    // as `// text`, silently rewriting `/* text */` and dropping
                    // its in-place position. Record it verbatim, by offset.
                    if src.as_bytes().get(lo) == Some(&b'/')
                        && src.as_bytes().get(lo + 1) == Some(&b'*')
                    {
                        let full = source_slice(src, lo, hi);
                        let own_line = lines.get(line).is_some_and(|l| l.trim() == full);
                        block.insert(
                            lo,
                            BlockComment {
                                end: hi,
                                line,
                                own_line,
                            },
                        );
                    } else if code_on_line == Some(line) {
                        line_comments.insert(lo);
                        trailing.insert(
                            line,
                            (text.as_str().to_string(), tok.span.col_range().0, lo),
                        );
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

    pub(crate) fn trailing_comment(&self, line: usize) -> Option<(&str, usize, usize)> {
        self.trailing
            .get(&line)
            .map(|(t, c, o)| (t.as_str(), *c, *o))
    }

    pub(crate) fn close_paren_after(&self, offset: usize) -> Option<(usize, usize)> {
        let (_, close) = self
            .code
            .range((Bound::Included(offset), Bound::Unbounded))
            .next()?;
        *close
    }

    pub(crate) fn standalone_comments_between(
        &self,
        after: usize,
        before: usize,
    ) -> impl Iterator<Item = (usize, &str)> {
        self.standalone
            .range((after + 1).min(before)..before)
            .map(|(line, text)| (*line, text.as_str()))
    }

    pub(crate) fn has_list_comments(&self, span: Span) -> bool {
        let (open, close) = span.line_range();
        let (lo, hi) = span.offset_range();
        self.standalone_comments_between(open, close)
            .next()
            .is_some()
            || self.has_block_comment_in(lo, hi)
    }

    pub(crate) fn inline_block_starting_after(&self, offset: usize) -> Option<(usize, usize)> {
        self.block
            .range((Bound::Included(offset), Bound::Unbounded))
            .find(|(_, b)| !b.own_line)
            .map(|(start, b)| (*start, b.end))
    }

    pub(crate) fn has_block_comment_in(&self, lo: usize, hi: usize) -> bool {
        self.block
            .range((Bound::Included(lo), Bound::Excluded(hi)))
            .next()
            .is_some()
    }

    pub(crate) fn has_inline_block_comment_in(&self, lo: usize, hi: usize) -> bool {
        self.block
            .range((Bound::Included(lo), Bound::Excluded(hi)))
            .any(|(_, b)| !b.own_line)
    }

    pub(crate) fn has_line_comment_in(&self, lo: usize, hi: usize) -> bool {
        self.line_comments
            .range((Bound::Included(lo), Bound::Excluded(hi)))
            .next()
            .is_some()
    }

    pub(crate) fn has_own_line_comment_between(&self, after: usize, before: usize) -> bool {
        self.standalone_comments_between(after, before)
            .next()
            .is_some()
            || self
                .own_line_block_comments_between(after, before)
                .next()
                .is_some()
    }

    pub(crate) fn block_comment_ending_before(&self, offset: usize) -> Option<(usize, usize)> {
        self.block
            .range((Bound::Unbounded, Bound::Included(offset)))
            .rev()
            .map(|(start, b)| (*start, b.end))
            .find(|(_, end)| *end <= offset)
    }

    pub(crate) fn own_line_block_comments_between(
        &self,
        after: usize,
        before: usize,
    ) -> impl Iterator<Item = (usize, usize, usize)> {
        (after + 1 < before)
            .then_some(&self.block)
            .into_iter()
            .flatten()
            .filter(move |(_, b)| b.own_line && b.line > after && b.line < before)
            .map(|(start, b)| (*start, b.end, b.line))
    }

    /// Number of fully blank lines strictly between two 0-based line numbers,
    /// capped at 2 (the formatter never emits more than two consecutive blanks).
    pub(crate) fn blank_count_between(&self, end_line: usize, start_line: usize) -> usize {
        if start_line <= end_line + 1 {
            return 0;
        }
        let n = ((end_line + 1)..start_line)
            .filter(|l| self.blank.get(*l).copied().unwrap_or(false))
            .count();
        n.min(2)
    }
}

pub(crate) fn source_slice(src: &str, lo: usize, hi: usize) -> &str {
    src.get(lo..hi).expect("invalid source span in formatter")
}
