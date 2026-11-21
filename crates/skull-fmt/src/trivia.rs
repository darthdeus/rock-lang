use rock_core::Span;
use rock_tokenizer::{Token, TokenKind};
use ustr::Ustr;

pub(crate) struct Trivia {
    comments: Vec<Comment>,
    blank: Vec<bool>,
    code: Vec<(usize, Option<Span>)>,
}

struct Comment {
    span: Span,
    text: Ustr,
    kind: CommentKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommentKind {
    Trailing,
    Standalone,
    InlineBlock,
    StandaloneBlock,
}

impl CommentKind {
    fn block(self) -> bool {
        matches!(self, Self::InlineBlock | Self::StandaloneBlock)
    }
}

impl Trivia {
    pub(crate) fn harvest(src: &str, tokens: &[Token]) -> Trivia {
        use CommentKind::*;
        let lines: Vec<_> = src.lines().map(str::trim).collect();
        let mut comments = Vec::new();
        let mut code = Vec::new();
        let mut code_on_line = None;
        for tok in tokens {
            let line = tok.span.line_range().0;
            let (lo, hi) = tok.span.offset_range();
            match tok.kind {
                TokenKind::Comment(text) => {
                    let source = source_slice(src, lo, hi);
                    let kind = if source.starts_with("/*") {
                        if lines.get(line) == Some(&source) {
                            StandaloneBlock
                        } else {
                            InlineBlock
                        }
                    } else if code_on_line == Some(line) {
                        Trailing
                    } else if lines.get(line).is_some_and(|line| line.starts_with("//")) {
                        Standalone
                    } else {
                        continue;
                    };
                    comments.push(Comment {
                        span: tok.span,
                        text,
                        kind,
                    });
                }
                kind => {
                    code_on_line = Some(line);
                    if !matches!(kind, TokenKind::Comma | TokenKind::Newline) {
                        let close = (kind == TokenKind::CloseParen).then_some(tok.span);
                        code.push((lo, close));
                    }
                }
            }
        }
        Self {
            comments,
            blank: lines.iter().map(|line| line.is_empty()).collect(),
            code,
        }
    }

    fn comments_in(&self, lo: usize, hi: usize) -> &[Comment] {
        let start = self
            .comments
            .partition_point(|c| c.span.offset_range().0 < lo);
        let end = self
            .comments
            .partition_point(|c| c.span.offset_range().0 < hi);
        &self.comments[start..end]
    }

    fn comments_after(&self, line: usize) -> &[Comment] {
        &self.comments[self
            .comments
            .partition_point(|c| c.span.line_range().0 <= line)..]
    }

    pub(crate) fn trailing_comment(&self, line: usize) -> Option<(&str, usize, usize)> {
        let end = self
            .comments
            .partition_point(|c| c.span.line_range().0 <= line);
        self.comments[..end]
            .iter()
            .rev()
            .take_while(|c| c.span.line_range().0 == line)
            .find(|c| c.kind == CommentKind::Trailing)
            .map(|c| {
                (
                    c.text.as_str(),
                    c.span.col_range().0,
                    c.span.offset_range().0,
                )
            })
    }

    pub(crate) fn close_paren_after(&self, offset: usize) -> Option<(usize, usize)> {
        let (_, close) = self
            .code
            .get(self.code.partition_point(|(at, _)| *at < offset))?;
        close.map(|span| (span.offset_range().1, span.line_range().1))
    }

    pub(crate) fn standalone_comments_between(
        &self,
        after: usize,
        before: usize,
    ) -> impl Iterator<Item = (usize, &str)> {
        self.comments_after(after)
            .iter()
            .take_while(move |c| c.span.line_range().0 < before)
            .filter(|c| c.kind == CommentKind::Standalone)
            .map(|c| (c.span.line_range().0, c.text.as_str()))
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
        self.comments_in(offset, usize::MAX)
            .iter()
            .find(|c| c.kind == CommentKind::InlineBlock)
            .map(|c| c.span.offset_range())
    }

    pub(crate) fn has_block_comment_in(&self, lo: usize, hi: usize) -> bool {
        self.comments_in(lo, hi).iter().any(|c| c.kind.block())
    }

    pub(crate) fn has_inline_block_comment_in(&self, lo: usize, hi: usize) -> bool {
        self.comments_in(lo, hi)
            .iter()
            .any(|c| c.kind == CommentKind::InlineBlock)
    }

    pub(crate) fn has_line_comment_in(&self, lo: usize, hi: usize) -> bool {
        self.comments_in(lo, hi).iter().any(|c| !c.kind.block())
    }

    pub(crate) fn has_own_line_comment_between(&self, after: usize, before: usize) -> bool {
        self.comments_after(after)
            .iter()
            .take_while(|c| c.span.line_range().0 < before)
            .any(|c| {
                matches!(
                    c.kind,
                    CommentKind::Standalone | CommentKind::StandaloneBlock
                )
            })
    }

    pub(crate) fn block_comment_ending_before(&self, offset: usize) -> Option<(usize, usize)> {
        self.comments_in(0, offset)
            .iter()
            .rev()
            .find(|c| c.kind.block() && c.span.offset_range().1 <= offset)
            .map(|c| c.span.offset_range())
    }

    pub(crate) fn own_line_block_comments_between(
        &self,
        after: usize,
        before: usize,
    ) -> impl Iterator<Item = (usize, usize, usize)> {
        self.comments_after(after)
            .iter()
            .take_while(move |c| c.span.line_range().0 < before)
            .filter(|c| c.kind == CommentKind::StandaloneBlock)
            .map(|c| {
                let (start, end) = c.span.offset_range();
                (start, end, c.span.line_range().0)
            })
    }

    pub(crate) fn blank_count_between(&self, end_line: usize, start_line: usize) -> usize {
        ((end_line + 1)..start_line)
            .filter(|l| self.blank.get(*l).copied().unwrap_or(false))
            .take(2)
            .count()
    }
}

pub(crate) fn source_slice(src: &str, lo: usize, hi: usize) -> &str {
    src.get(lo..hi).expect("invalid source span in formatter")
}
