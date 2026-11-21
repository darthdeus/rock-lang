use crate::ast::*;
use rock_core::nesting::MAX_NESTING_DEPTH;
use rock_core::{CompilerError, Span};
use rock_tokenizer::{BindingPower, Token, TokenKind, Tokenizer, interp};
use std::rc::Rc;
use ustr::Ustr;

fn is_lua_identifier(segment: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if",
        "in", "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
    ];
    let mut bytes = segment.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !KEYWORDS.contains(&segment)
}

pub(crate) struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    eof_span: Span,
    ast: Ast,
    /// Depth inside `(` / `[` — a newline does not separate inside these.
    delimiter_depth: i32,
    /// Depth inside `{ }` code blocks — a newline DOES separate here.
    brace_depth: i32,
    /// Whether `Name { ... }` is a struct literal here. False while parsing
    /// `if`/`while`/`for`/`match` condition heads (where `{` opens the body),
    /// true in delimited contexts and statement/value position.
    allow_literals: bool,
    /// True while parsing a no-parens call's argument list, to block recursive
    /// no-parens (`f a b` is `f(a, b)`, not `f(a(b))`).
    in_no_parens_args: bool,
    /// Keep the match-arm delimiter `=>` from parsing as a lambda.
    arrow_lambda_blocked: bool,
    /// Set by `parse_operator` so `parse_item` can replicate rock's implicit
    /// `@inline` on block-bodied operator overloads (the `@_transparent` model).
    last_was_operator: bool,
    hole_counter: u32,
    /// Depth of paren groups whose head is currently parsing. While > 0 the
    /// Pratt loop breaks at `:` so the paren arm owns the `(expr : Type)`
    /// ascription form instead of `:` binding as a binop.
    paren_head: u32,
    nesting: u32,
    /// Height, in nesting levels, of the deepest expression or block completed
    /// within the innermost level still open; see [`Parser::nesting_level`].
    nested_height: u32,
    /// Set when the nesting limit is hit, so statement recovery propagates
    /// the error to the item instead of resuming at the same depth.
    too_deep: bool,
    source: &'a str,
    source_base: usize,
    holes: Vec<Rc<HoleFrame>>,
}

struct HoleFrame {
    text: String,
    /// `map[k]` is the file offset of `text[k]`, with a trailing end entry.
    map: Vec<usize>,
    line: usize,
    column: usize,
}

#[derive(Clone, Copy)]
struct Pos {
    at: usize,
    line: usize,
    column: usize,
}

impl Pos {
    /// Move forward to byte offset `to`, counting newlines. Backwards moves are
    /// a no-op -- pieces are visited in source order.
    fn advance(&mut self, text: &str, to: usize) {
        if to <= self.at {
            return;
        }
        let skipped = &text[self.at..to];
        match skipped.rfind('\n') {
            Some(last) => {
                self.line += skipped.matches('\n').count();
                self.column = skipped[last + 1..].chars().count();
            }
            None => self.column += skipped.chars().count(),
        }
        self.at = to;
    }
}

const STATIC_PATTERN_ERROR: &str =
    "a `static` takes a plain name, not a pattern: give it one name and destructure from it";

fn identity_map(base: usize, len: usize) -> Vec<usize> {
    (base..=base + len).collect()
}

/// `{{` / `}}` stand for a single literal brace. The tokenizer keeps them
/// doubled (its content is what non-interpolation consumers read); collapsing
/// is the interpolation parse's step.
fn collapse_brace_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if (c == '{' || c == '}') && chars.peek() == Some(&c) {
            chars.next();
        }
        out.push(c);
    }
    out
}

type PResult<T> = Result<T, CompilerError>;

impl<'a> Parser<'a> {
    /// `source` is the text `tokens` were produced from, and `base` the file
    /// offset of its first byte. Interpolated strings are re-scanned from it, so
    /// a caller that passes the wrong text gets spanned errors, not silent
    /// drift.
    pub(crate) fn new(tokens: Vec<Token>, source: &'a str, base: usize) -> Self {
        let eof_span = tokens.last().map(|t| t.span).unwrap_or_else(Span::unknown);
        let ast = Ast::with_capacity(tokens.len());
        Self {
            source,
            source_base: base,
            holes: Vec::new(),
            tokens,
            pos: 0,
            eof_span,
            ast,
            delimiter_depth: 0,
            brace_depth: 0,
            allow_literals: true,
            in_no_parens_args: false,
            arrow_lambda_blocked: false,
            last_was_operator: false,
            hole_counter: 0,
            paren_head: 0,
            nesting: 0,
            nested_height: 0,
            too_deep: false,
        }
    }

    fn gen_hole_id(&mut self) -> u32 {
        let id = self.hole_counter;
        self.hole_counter += 1;
        id
    }

    fn with_allow_literals<T>(&mut self, allow: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.allow_literals, allow);
        let out = f(self);
        self.allow_literals = saved;
        out
    }

    /// Run `f` inside a paren group head: the Pratt loop breaks at `:` there
    /// (see the `paren_head` field). The restore runs on every path out of
    /// `f`, so statement-level recovery never sees a stale depth.
    fn with_paren_head<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.paren_head += 1;
        let out = f(self);
        self.paren_head -= 1;
        out
    }

    /// Recoverable syntax errors are collected in the AST.
    pub fn parse_module(mut self) -> PResult<Ast> {
        let mut seen_item = false;
        loop {
            self.skip_comments_as_items();
            if self.at_end() {
                break;
            }
            if !seen_item {
                let save = self.pos;
                let start = self.span();
                let mods = self.parse_vis_mods()?;
                if self.at_mod_decl() {
                    self.advance(); // `mod`
                    let mut path = Vec::new();
                    loop {
                        let seg = self.expect_ident()?;
                        if seg.name.as_str() == "self"
                            && path.is_empty()
                            && self.kind() == TokenKind::Semicolon
                        {
                            break;
                        }
                        path.push(seg);
                        if self.kind() != TokenKind::DoubleColon {
                            break;
                        }
                        self.advance();
                    }
                    let span = self.span_from(start);
                    if self.kind() == TokenKind::OpenCurly {
                        self.pos = save;
                    } else if self.kind() != TokenKind::Semicolon {
                        return Err(self.err(
                            "expected `;` after the file header path (or `{` for an inline block)",
                        ));
                    } else if self.ast.module_header.is_some() {
                        return Err(CompilerError::new_with_label(
                            span,
                            "a file has one `mod path;` header; a second one would move the rest of the file to another module".to_string(),
                        ));
                    } else {
                        self.advance(); // `;`
                        self.ast.module_header = Some(ModuleHeader {
                            path,
                            vis: mods.vis,
                            span,
                        });
                        continue;
                    }
                }
                self.pos = save;
            }
            seen_item = true;
            let item_start = self.pos;
            match self.parse_item() {
                Ok(item) => self.ast.items.push(item),
                Err(e) => {
                    self.ast.recovery_errors.push(e);
                    self.too_deep = false;
                    self.recover_to_next_item(self.pos > item_start);
                }
            }
        }
        Ok(self.ast)
    }

    pub(crate) fn parse_single_item(mut self) -> PResult<Ast> {
        self.skip_comments_as_items();
        let item = self.parse_item()?;
        self.ast.items.push(item);
        Ok(self.ast)
    }

    fn recover_to_next_item(&mut self, made_progress: bool) {
        let mut depth: i32 = 0;
        if !made_progress && !self.at_end() {
            self.advance_tracking_depth(&mut depth);
        }
        while !self.at_end() {
            if depth <= 0 && self.kind() == TokenKind::Fn {
                return;
            }
            self.advance_tracking_depth(&mut depth);
        }
    }

    fn advance_tracking_depth(&mut self, depth: &mut i32) {
        match self.kind() {
            TokenKind::OpenCurly | TokenKind::OpenParen | TokenKind::OpenSquare => *depth += 1,
            TokenKind::CloseCurly | TokenKind::CloseParen | TokenKind::CloseSquare => *depth -= 1,
            _ => {}
        }
        self.advance();
    }

    fn can_start_statement(kind: TokenKind) -> bool {
        matches!(
            kind,
            TokenKind::Let
                | TokenKind::Return
                | TokenKind::Defer
                | TokenKind::If
                | TokenKind::For
                | TokenKind::While
                | TokenKind::Loop
                | TokenKind::Yield
                | TokenKind::Await
                | TokenKind::Break
                | TokenKind::Continue
                | TokenKind::OpenCurly
                | TokenKind::Ident(_)
                | TokenKind::Int(_, _)
                | TokenKind::Float(_)
                | TokenKind::String(_)
                | TokenKind::RawString(_)
                | TokenKind::True
                | TokenKind::False
                | TokenKind::Null
                | TokenKind::OpenParen
                | TokenKind::OpenSquare
                | TokenKind::Minus
                | TokenKind::Bang
                | TokenKind::PlusPlus
                | TokenKind::MinusMinus
                | TokenKind::BitXor
                | TokenKind::Match
                | TokenKind::Global
                | TokenKind::Const
                | TokenKind::At
                | TokenKind::Comment(_)
        )
    }

    fn recover_to_next_statement(&mut self) -> bool {
        while !self.at_end() {
            match self.kind() {
                TokenKind::Semicolon => {
                    self.advance();
                    return true;
                }
                TokenKind::CloseCurly | TokenKind::EOF => return false,
                k if Self::can_start_statement(k) => return true,
                _ => self.advance(),
            }
        }
        false
    }

    fn kind(&self) -> TokenKind {
        self.tokens
            .get(self.pos)
            .map(|t| t.kind)
            .unwrap_or(TokenKind::EOF)
    }

    fn span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .map(|t| t.span)
            .unwrap_or(self.eof_span)
    }

    /// Span of the most-recently-consumed token (`pos - 1`). A node's full
    /// extent runs from its first token (`start`, captured before consuming it)
    /// through this — `self.span()` is the *next*, unconsumed token, so it
    /// overshoots.
    fn prev_span(&self) -> Span {
        if self.pos == 0 {
            self.span()
        } else {
            self.tokens
                .get(self.pos - 1)
                .map(|t| t.span)
                .unwrap_or(self.eof_span)
        }
    }

    /// Merge into one span covering `[start.start, end.end]`, guarding the
    /// file-mismatch / unknown cases `Span::merge` would panic on.
    fn merge_spans(&self, start: Span, end: Span) -> Span {
        if start.file == end.file && start.file.as_str() != "<unknown>" {
            start.merge(&end)
        } else {
            start
        }
    }

    fn span_from(&self, start: Span) -> Span {
        self.merge_spans(start, self.prev_span())
    }

    fn peek_kind(&self) -> TokenKind {
        self.tokens
            .get(self.pos + 1)
            .map(|t| t.kind)
            .unwrap_or(TokenKind::EOF)
    }

    /// Newlines that followed the *previous* token (the tokenizer folds
    /// `Newline` into `Token::trailing_newlines`).
    fn prev_trailing_newlines(&self) -> usize {
        self.pos
            .checked_sub(1)
            .and_then(|i| self.tokens.get(i))
            .map_or(0, |t| t.trailing_newlines)
    }

    fn at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn advance(&mut self) {
        self.pos += 1;
    }

    fn eat(&mut self, kind: TokenKind) -> bool {
        if self.kind() == kind {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: TokenKind) -> PResult<Span> {
        if self.kind() == kind {
            let span = self.span();
            self.advance();
            Ok(span)
        } else {
            Err(self.err(format!("expected {:?}, got {:?}", kind, self.kind())))
        }
    }

    fn err(&self, msg: impl Into<String>) -> CompilerError {
        CompilerError::new_with_label(self.span(), msg.into())
    }

    /// Consume the `>` closing a generic argument list. A nested close like
    /// `Map<K, Vec<V>>` tokenizes the trailing `>>` as a single `RightShift`;
    /// the inner close consumes that token and reinserts one `>` for the
    /// enclosing list (mirrors rock's `expect_generic_close`).
    fn expect_generic_close(&mut self) -> PResult<Span> {
        match self.kind() {
            TokenKind::RightShift => {
                let span = self.span();
                self.advance();
                self.tokens.insert(
                    self.pos,
                    Token {
                        kind: TokenKind::GreaterThan,
                        span,
                        trailing_newlines: 0,
                    },
                );
                Ok(span)
            }
            _ => self.expect(TokenKind::GreaterThan),
        }
    }

    fn skip_comments(&mut self) {
        while matches!(self.kind(), TokenKind::Comment(_)) {
            self.advance();
        }
    }

    fn skip_comments_as_items(&mut self) {
        while let TokenKind::Comment(text) = self.kind() {
            let span = self.span();
            self.ast.items.push(Item {
                annotations: Vec::new(),
                vis: None,
                span,
                kind: ItemKind::Comment(text),
            });
            self.advance();
        }
        while self.eat(TokenKind::Semicolon) {}
    }

    fn expect_ident(&mut self) -> PResult<Ident> {
        match self.kind() {
            TokenKind::Ident(name) => {
                let span = self.span();
                self.advance();
                Ok(Ident { span, name })
            }
            other => Err(self.err(format!("expected identifier, got {:?}", other))),
        }
    }

    fn at_list_close(&self, close: TokenKind) -> bool {
        let k = self.kind();
        k == close
            || (close == TokenKind::GreaterThan && k == TokenKind::RightShift)
            || self.at_end()
    }

    fn list_item_follows(&mut self, close: TokenKind) -> bool {
        self.skip_comments();
        !self.at_list_close(close)
    }

    fn list_continues(&mut self) -> bool {
        self.skip_comments();
        self.eat(TokenKind::Comma)
    }

    /// Leave the closing token for the caller.
    fn parse_comma_list<T>(
        &mut self,
        close: TokenKind,
        mut parse_item: impl FnMut(&mut Self) -> PResult<T>,
    ) -> PResult<Vec<T>> {
        let mut items = Vec::new();
        while self.list_item_follows(close) {
            items.push(parse_item(self)?);
            if !self.list_continues() {
                break;
            }
        }
        Ok(items)
    }

    /// Leave comments for the element parser or the closing-token diagnostic.
    fn parse_bare_comma_list<T>(
        &mut self,
        close: TokenKind,
        mut parse_item: impl FnMut(&mut Self) -> PResult<T>,
    ) -> PResult<Vec<T>> {
        let mut items = Vec::new();
        while !self.at_list_close(close) {
            items.push(parse_item(self)?);
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        Ok(items)
    }

    /// Visibility is contextual: `private: bool` remains a field.
    fn parse_vis_mods(&mut self) -> PResult<VisMods> {
        let mut mods = VisMods::default();
        let peek2 = |p: &Self| {
            p.tokens
                .get(p.pos + 2)
                .map(|t| t.kind)
                .unwrap_or(TokenKind::EOF)
        };
        let starts_item_like = |k: &TokenKind| {
            matches!(
                k,
                TokenKind::Ident(_)
                    | TokenKind::At
                    | TokenKind::Deco
                    | TokenKind::Fn
                    | TokenKind::Struct
                    | TokenKind::Enum
                    | TokenKind::Newtype
                    | TokenKind::Global
                    | TokenKind::Const
                    | TokenKind::Operator
                    | TokenKind::Extern
                    | TokenKind::DoubleColon
                    | TokenKind::OpenCurly
            )
        };
        loop {
            let is_pub = matches!(self.kind(), TokenKind::Ident(n) if n.as_str() == "pub");
            let is_private = matches!(self.kind(), TokenKind::Ident(n) if n.as_str() == "private");
            // `in` is a keyword token (for loops), not an Ident.
            let pub_follows = starts_item_like(&self.peek_kind())
                || (self.peek_kind() == TokenKind::OpenParen
                    && match peek2(self) {
                        TokenKind::In => true,
                        TokenKind::Ident(n) => n.as_str() == "package" || n.as_str() == "super",
                        _ => false,
                    });
            let private_follows = starts_item_like(&self.peek_kind())
                || (self.peek_kind() == TokenKind::OpenParen
                    && matches!(peek2(self), TokenKind::Ident(ref n) if n.as_str() == "set"));
            if is_pub && pub_follows {
                self.advance();
                if self.kind() == TokenKind::OpenParen && self.peek_kind() == TokenKind::In {
                    self.advance(); // `(`
                    self.advance(); // `in`
                    let mut segments = vec![self.expect_ident()?];
                    while self.kind() == TokenKind::DoubleColon {
                        self.advance();
                        segments.push(self.expect_ident()?);
                    }
                    self.expect(TokenKind::CloseParen)?;
                    mods.vis = Some(Visibility::PubIn(segments));
                } else if self.kind() == TokenKind::OpenParen {
                    self.advance();
                    let q = self.expect_ident()?;
                    match q.name.as_str() {
                        "package" => {
                            self.expect(TokenKind::CloseParen)?;
                            mods.vis = Some(Visibility::PubPackage);
                        }
                        "super" => {
                            self.expect(TokenKind::CloseParen)?;
                            mods.vis = Some(Visibility::PubSuper);
                        }
                        other => {
                            return Err(self.err(format!(
                                "unknown visibility qualifier `pub({other})`; known: pub, pub(super), pub(package), pub(in path)"
                            )));
                        }
                    }
                } else {
                    mods.vis = Some(Visibility::Pub);
                }
                continue;
            }
            if is_private && private_follows {
                self.advance();
                if self.kind() == TokenKind::OpenParen
                    && matches!(self.peek_kind(), TokenKind::Ident(n) if n.as_str() == "set")
                {
                    self.advance(); // `(`
                    self.advance(); // `set`
                    self.expect(TokenKind::CloseParen)?;
                    mods.set_private = true;
                } else {
                    mods.vis = Some(Visibility::Private);
                }
                continue;
            }
            break;
        }
        Ok(mods)
    }

    fn parse_use_decl(&mut self, start: Span, vis: Option<Visibility>) -> PResult<UseDecl> {
        debug_assert!(matches!(self.kind(), TokenKind::Ident(n) if n.as_str() == "use"));
        self.advance(); // `use`
        self.skip_comments();
        let from_root = self.eat(TokenKind::DoubleColon);
        let mut paths = Vec::new();
        self.parse_use_path(None, &mut paths)?;
        self.eat(TokenKind::Semicolon);
        let span = self.span_from(start);
        Ok(UseDecl {
            span,
            vis,
            from_root,
            paths: paths.into(),
        })
    }

    fn parse_use_path(&mut self, parent: Option<usize>, paths: &mut Vec<UsePath>) -> PResult<()> {
        let mut segments = Vec::new();
        loop {
            if self.kind() == TokenKind::OpenCurly
                || self.kind() == TokenKind::Mul
                || self.suffix_is_operator_selector()
            {
                break;
            }
            segments.push(self.expect_ident()?);
            if self.kind() != TokenKind::DoubleColon {
                break;
            }
            self.advance(); // `::`
        }
        if self.eat(TokenKind::OpenCurly) {
            let group = Some(paths.len());
            paths.push(UsePath {
                parent,
                segments,
                suffix: UseSuffix::Group,
            });
            self.parse_comma_list(TokenKind::CloseCurly, |p| p.parse_use_path(group, paths))?;
            self.expect(TokenKind::CloseCurly)?;
            return Ok(());
        }
        let suffix = if self.eat(TokenKind::Mul) {
            UseSuffix::Glob
        } else if self.suffix_is_operator_selector() {
            // The operator selector's glyph is a token, not an identifier.
            self.advance(); // `operator`
            self.expect(TokenKind::OpenParen)?;
            let glyph_span = self.span();
            let glyph = self.kind();
            self.advance();
            self.expect(TokenKind::CloseParen)?;
            let op_fn = glyph
                .overloadable_binop_fn()
                .or(match glyph {
                    TokenKind::Bang => Some("__not"),
                    _ => None,
                })
                .ok_or_else(|| {
                    self.err(format!(
                        "operator({:?}) is not an importable operator family",
                        glyph
                    ))
                })?;
            segments.push(Ident {
                span: glyph_span,
                name: Ustr::from(op_fn),
            });
            UseSuffix::Operator(self.eat_use_alias()?)
        } else {
            UseSuffix::Bind(self.eat_use_alias()?)
        };
        paths.push(UsePath {
            parent,
            segments,
            suffix,
        });
        Ok(())
    }

    fn eat_use_alias(&mut self) -> PResult<Option<Ident>> {
        (self.kind() == TokenKind::As)
            .then(|| self.parse_use_alias())
            .transpose()
    }

    fn parse_use_alias(&mut self) -> PResult<Ident> {
        self.expect(TokenKind::As)?;
        if matches!(self.kind(), TokenKind::Ident(n) if n.as_str() == "_") {
            return Err(self.err(
                "`use ... as _` has no activation semantics in modules-v1 and is not supported",
            ));
        }
        self.expect_ident()
    }

    /// At `operator` + `(` in a use suffix position. `operator` is a keyword
    /// token; it never appears as a path segment spelling.
    fn suffix_is_operator_selector(&self) -> bool {
        self.kind() == TokenKind::Operator && self.peek_kind() == TokenKind::OpenParen
    }

    fn parse_mod_block(
        &mut self,
        start: Span,
        vis: Option<Visibility>,
        name: Ident,
    ) -> PResult<Item> {
        debug_assert!(self.kind() == TokenKind::OpenCurly);
        self.advance(); // `{`
        self.brace_depth += 1;
        let mut items = Vec::new();
        loop {
            self.skip_comments_as_items();
            if self.kind() == TokenKind::CloseCurly || self.at_end() {
                break;
            }
            items.push(self.parse_item()?);
        }
        self.brace_depth -= 1;
        let close = self.expect(TokenKind::CloseCurly)?;
        let span = self.merge_spans(start, close);
        Ok(Item {
            annotations: Vec::new(),
            vis: vis.clone(),
            span,
            kind: ItemKind::Module(ModBlock {
                span,
                name,
                vis,
                items,
            }),
        })
    }

    fn at_mod_decl(&self) -> bool {
        matches!(self.kind(), TokenKind::Ident(n) if n.as_str() == "mod")
            && matches!(self.peek_kind(), TokenKind::Ident(_))
    }

    fn at_use_decl(&self) -> bool {
        matches!(self.kind(), TokenKind::Ident(n) if n.as_str() == "use")
            && (matches!(self.peek_kind(), TokenKind::Ident(_))
                || self.peek_kind() == TokenKind::DoubleColon)
    }

    fn parse_item(&mut self) -> PResult<Item> {
        let item_start = self.span();
        let mods = self.parse_vis_mods()?;
        if mods.set_private {
            return Err(self.err("`private(set)` applies to stored struct fields, not to items"));
        }
        if self.at_use_decl() {
            let decl = self.parse_use_decl(item_start, mods.vis.clone())?;
            return Ok(Item {
                annotations: Vec::new(),
                vis: mods.vis,
                span: decl.span,
                kind: ItemKind::Use(decl),
            });
        }
        if self.at_mod_decl() {
            self.advance(); // `mod`
            let name = self.expect_ident()?;
            if matches!(self.kind(), TokenKind::Semicolon | TokenKind::DoubleColon) {
                return Err(self.err(
                    "`mod path;` is a file header and must be the file's first item; inline module blocks use `{ .. }`",
                ));
            }
            if self.kind() != TokenKind::OpenCurly {
                return Err(self.err("expected `{` after the inline module name"));
            }
            return self.parse_mod_block(item_start, mods.vis, name);
        }
        let mut annotations = self.parse_item_annotations()?;
        self.last_was_operator = false;
        let kind = self.parse_item_inner()?;
        // rock injects `@inline` on block-bodied operator overloads (no source
        // form) unless one is already present.
        if self.last_was_operator
            && let ItemKind::Function(f) = &kind
            && f.body.rock_block().is_some()
            && !annotations.iter().any(|a| a.name.name.as_str() == "inline")
        {
            annotations.push(Annotation {
                span: f.span,
                name: Ident {
                    span: f.span,
                    name: Ustr::from("inline"),
                },
                args: Vec::new(),
            });
        }
        Ok(Item {
            annotations,
            vis: mods.vis,
            span: self.span_from(item_start),
            kind,
        })
    }

    fn parse_item_inner(&mut self) -> PResult<ItemKind> {
        match self.kind() {
            TokenKind::Fn => self.parse_fn(false, None, FnBackend::Lua),
            // `co fn name(...)` — coroutine; body may use `yield`. `co` is a
            // contextual keyword: only reserved immediately before `fn`.
            TokenKind::Ident(n)
                if n.as_str() == "co" && matches!(self.peek_kind(), TokenKind::Fn) =>
            {
                self.advance();
                self.parse_fn(true, None, FnBackend::Lua)
            }
            TokenKind::Operator => self.parse_operator(false, None, FnBackend::Lua),
            TokenKind::Macro => {
                let start = self.span();
                self.advance();
                let name = self.expect_ident()?;
                self.finish_fn_def(name, start, true, false, None, FnBackend::Lua)
                    .map(ItemKind::Function)
            }
            TokenKind::Extern => {
                self.advance();
                let extern_body = self.parse_extern_specifier()?;
                self.parse_fn(false, Some(extern_body), FnBackend::Lua)
            }
            TokenKind::Ident(n) if n.as_str() == "inline" => {
                self.advance();
                let backend = self.parse_inline_backend()?;
                match self.kind() {
                    TokenKind::Operator => self.parse_operator(false, None, backend),
                    TokenKind::Ident(n) if n.as_str() == "co" => {
                        self.advance();
                        self.parse_fn(true, None, backend)
                    }
                    _ => self.parse_fn(false, None, backend),
                }
            }
            TokenKind::Struct => self.parse_struct(false, false),
            TokenKind::Ident(n)
                if n.as_str() == "value" && matches!(self.peek_kind(), TokenKind::Ident(_)) =>
            {
                self.parse_struct(true, false)
            }
            TokenKind::Ident(n)
                if n.as_str() == "nobox"
                    && matches!(self.peek_kind(), TokenKind::Ident(v) if v.as_str() == "value") =>
            {
                self.parse_struct(true, true)
            }
            TokenKind::Ident(n)
                if n.as_str() == "nobox"
                    && !matches!(self.peek_kind(), TokenKind::OpenParen | TokenKind::Bang) =>
            {
                Err(self.err("`nobox` requires `value`"))
            }
            TokenKind::Enum => self.parse_enum(),
            TokenKind::Newtype => self.parse_newtype(),
            TokenKind::Deco => self.parse_decorator(),
            TokenKind::Global => self.parse_global(false),
            TokenKind::Const => self.parse_global(true),
            TokenKind::DirectiveIf
            | TokenKind::DirectiveElseIf
            | TokenKind::DirectiveElse
            | TokenKind::DirectiveEnd => Ok(self.parse_compiler_directive()),
            TokenKind::Ident(_)
                if matches!(self.peek_kind(), TokenKind::OpenParen | TokenKind::Bang) =>
            {
                self.parse_top_level_invocation()
            }
            other => Err(self.err(format!("unsupported item start: {:?}", other))),
        }
    }

    fn parse_top_level_invocation(&mut self) -> PResult<ItemKind> {
        let name = self.expect_ident()?;
        let sigil = self.eat(TokenKind::Bang);
        self.expect(TokenKind::OpenParen)?;
        let args = self.parse_comma_list(TokenKind::CloseParen, |p| p.parse_macro_arg(sigil))?;
        self.expect(TokenKind::CloseParen)?;
        Ok(ItemKind::MacroInvocation { name, args, sigil })
    }

    fn parse_sigil_call(
        &mut self,
        func: Idx<ExprData>,
        receiver: Option<Idx<ExprData>>,
    ) -> PResult<Idx<ExprData>> {
        let start = self.ast.exprs[receiver.unwrap_or(func)].span;
        self.expect(TokenKind::OpenParen)?;
        self.delimiter_depth += 1;
        let mut args = self.parse_comma_list(TokenKind::CloseParen, |p| {
            let is_splat = p.eat(TokenKind::TripleDot);
            Ok(FunctionCallArg {
                is_splat,
                ..FunctionCallArg::positional(p.parse_macro_arg(true)?)
            })
        })?;
        self.delimiter_depth -= 1;
        self.expect(TokenKind::CloseParen)?;
        let syntax = if let Some(receiver) = receiver {
            args.insert(0, FunctionCallArg::positional(receiver));
            CallSyntax::MethodMacro
        } else {
            CallSyntax::Macro
        };
        Ok(self.ast.expr(
            self.span_from(start),
            ExprKind::FunctionCall {
                func,
                generic_args: Vec::new(),
                args,
                syntax,
            },
        ))
    }

    fn parse_macro_arg(&mut self, sigil: bool) -> PResult<Idx<ExprData>> {
        if !sigil {
            return self.with_allow_literals(true, |p| p.parse_expr(0));
        }
        // A type-shaped parse must end at an argument boundary; otherwise
        // `(a < b)` can be mistaken for a generic.
        let saved = self.pos;
        if let Ok(e) = self.parse_sigil_type_arg() {
            if matches!(
                self.kind(),
                TokenKind::Comma
                    | TokenKind::CloseParen
                    | TokenKind::CloseSquare
                    | TokenKind::GreaterThan
                    | TokenKind::RightShift
            ) {
                return Ok(e);
            }
        }
        self.pos = saved;
        self.with_allow_literals(true, |p| p.parse_expr(0))
    }

    fn parse_sigil_type_arg(&mut self) -> PResult<Idx<ExprData>> {
        let start = self.span();
        let mut e = self.parse_sigil_atom()?;
        while self.kind() == TokenKind::QuestionMark {
            self.advance();
            let span = self.span_from(start);
            e = self.ast.expr(span, ExprKind::QuestionMark(e));
        }
        Ok(e)
    }

    fn parse_sigil_atom(&mut self) -> PResult<Idx<ExprData>> {
        let start = self.span();
        match self.kind() {
            TokenKind::Ident(name) if self.peek_kind() == TokenKind::LessThan => {
                self.advance(); // name
                self.advance(); // `<`
                let func = self
                    .ast
                    .expr(start, ExprKind::Ident(Ident { span: start, name }));
                let args = self.parse_bare_comma_list(TokenKind::GreaterThan, |p| {
                    p.parse_macro_arg(true).map(FunctionCallArg::positional)
                })?;
                self.expect_generic_close()?;
                let span = self.span_from(start);
                Ok(self.ast.expr(
                    span,
                    ExprKind::FunctionCall {
                        func,
                        generic_args: Vec::new(),
                        args,
                        syntax: CallSyntax::TypeArgs,
                    },
                ))
            }
            open @ (TokenKind::OpenSquare | TokenKind::OpenParen) => {
                self.advance();
                let close = if open == TokenKind::OpenSquare {
                    TokenKind::CloseSquare
                } else {
                    TokenKind::CloseParen
                };
                let elems = self.parse_bare_comma_list(close, |p| p.parse_macro_arg(true))?;
                self.expect(close)?;
                let span = self.span_from(start);
                let kind = if open == TokenKind::OpenSquare {
                    ExprKind::ArrayLiteral(elems)
                } else if elems.len() == 1 {
                    ExprKind::Paren(elems[0])
                } else {
                    ExprKind::Tuple(elems)
                };
                Ok(self.ast.expr(span, kind))
            }
            _ => self.with_allow_literals(true, |p| p.parse_expr(26)),
        }
    }

    fn parse_extern_specifier(&mut self) -> PResult<FunctionBody> {
        self.expect(TokenKind::OpenParen)?;
        let kind = match self.kind() {
            TokenKind::Ident(n) if n.as_str() == "ffi" => {
                self.advance();
                self.eat(TokenKind::Comma);
                match self.kind() {
                    TokenKind::String(lib) | TokenKind::RawString(lib) => {
                        self.advance();
                        FunctionBody::Ffi(lib)
                    }
                    _ => FunctionBody::Ffi(Ustr::from("")),
                }
            }
            TokenKind::Ident(n) if n.as_str() == "lua" => {
                self.advance();
                let path = if self.eat(TokenKind::Comma) {
                    let (TokenKind::String(path) | TokenKind::RawString(path)) = self.kind() else {
                        return Err(self.err("expected a Lua symbol path string"));
                    };
                    if !path.split('.').all(is_lua_identifier) {
                        return Err(
                            self.err("expected a Lua symbol path of dot-separated identifiers")
                        );
                    }
                    self.advance();
                    Some(path)
                } else {
                    None
                };
                self.expect(TokenKind::CloseParen)?;
                return Ok(FunctionBody::ExternLua(path));
            }
            TokenKind::Ident(n) if n.as_str() == "js" => {
                self.advance();
                FunctionBody::ExternJs
            }
            other => {
                return Err(self.err(format!(
                    "unknown extern backend {:?}; expected `ffi`, `lua`, or `js`",
                    other
                )));
            }
        };
        while !matches!(self.kind(), TokenKind::CloseParen | TokenKind::EOF) {
            self.advance();
        }
        self.eat(TokenKind::CloseParen);
        Ok(kind)
    }

    fn parse_item_annotations(&mut self) -> PResult<Vec<Annotation>> {
        let mut anns = Vec::new();
        while self.kind() == TokenKind::At {
            let start = self.span(); // `@`
            self.advance(); // `@`
            let name = self.expect_ident()?;
            let mut args = Vec::new();
            if self.kind() == TokenKind::OpenParen {
                self.advance();
                self.delimiter_depth += 1;
                args = self.parse_comma_list(TokenKind::CloseParen, |p| {
                    let arg_name = if matches!(p.kind(), TokenKind::Ident(_))
                        && p.peek_kind() == TokenKind::Assign
                    {
                        let n = p.expect_ident()?;
                        p.expect(TokenKind::Assign)?;
                        Some(n)
                    } else {
                        None
                    };
                    let value = p.with_allow_literals(true, |p| p.parse_expr(0))?;
                    Ok(AnnoArg {
                        name: arg_name,
                        value,
                    })
                })?;
                self.delimiter_depth -= 1;
                self.expect(TokenKind::CloseParen)?;
            }
            let span = self.span_from(start);
            anns.push(Annotation { span, name, args });
            self.skip_comments();
        }
        Ok(anns)
    }

    fn parse_inline_backend(&mut self) -> PResult<FnBackend> {
        if self.kind() != TokenKind::OpenParen {
            return Ok(FnBackend::Lua);
        }
        self.advance(); // `(`
        let backend = match self.kind() {
            TokenKind::Ident(n) if n.as_str() == "lua" => FnBackend::Lua,
            TokenKind::Ident(n) if n.as_str() == "js" => FnBackend::Js,
            other => {
                return Err(self.err(format!(
                    "unknown inline backend {:?}; expected `lua` or `js`",
                    other
                )));
            }
        };
        self.advance(); // backend name
        self.expect(TokenKind::CloseParen)?;
        Ok(backend)
    }

    fn parse_fn(
        &mut self,
        is_coroutine: bool,
        extern_body: Option<FunctionBody>,
        inline_backend: FnBackend,
    ) -> PResult<ItemKind> {
        let start = self.span();
        self.expect(TokenKind::Fn)?;
        let name = self.expect_ident()?;
        self.finish_fn_def(
            name,
            start,
            false,
            is_coroutine,
            extern_body,
            inline_backend,
        )
        .map(ItemKind::Function)
    }

    fn parse_operator(
        &mut self,
        is_coroutine: bool,
        extern_body: Option<FunctionBody>,
        inline_backend: FnBackend,
    ) -> PResult<ItemKind> {
        self.last_was_operator = true;
        let start = self.span();
        self.expect(TokenKind::Operator)?;
        if self.kind() == TokenKind::OpenParen {
            self.advance();
            while self.kind() != TokenKind::CloseParen && !self.at_end() {
                self.advance();
            }
            self.expect(TokenKind::CloseParen)?;
        }
        self.skip_comments();
        let sym_span = self.span();
        let symbol = self.kind();
        let op_name = match symbol {
            TokenKind::Plus => "__op_add",
            TokenKind::Minus => "__op_sub",
            TokenKind::Mul => "__op_mul",
            TokenKind::Div => "__op_div",
            TokenKind::IntDiv => "__op_idiv",
            TokenKind::Mod => "__op_mod",
            TokenKind::Equals => "__op_eq",
            TokenKind::NotEquals => "__op_ne",
            TokenKind::LessThan => "__op_lt",
            TokenKind::LessThanEqual => "__op_le",
            TokenKind::GreaterThan => "__op_gt",
            TokenKind::GreaterThanEqual => "__op_ge",
            TokenKind::And => "__op_and",
            TokenKind::Or => "__op_or",
            TokenKind::Bang => "__op_not",
            TokenKind::BitNot => "__op_bitnot",
            TokenKind::BitAnd => "__op_bitand",
            TokenKind::BitOr => "__op_bitor",
            TokenKind::BitXor => "__op_bitxor",
            TokenKind::LeftShift => "__op_shl",
            TokenKind::RightShift => "__op_shr",
            TokenKind::OpenSquare if self.peek_kind() == TokenKind::CloseSquare => {
                self.advance(); // `[`
                "__op_index"
            }
            other => return Err(self.err(format!("expected operator symbol, got {:?}", other))),
        };
        self.advance(); // the symbol token (or `]` for index)
        let name = Ident {
            span: sym_span,
            name: Ustr::from(op_name),
        };
        let mut function = self.finish_fn_def(
            name,
            start,
            false,
            is_coroutine,
            extern_body,
            inline_backend,
        )?;
        if function.params.len() == 1 {
            match symbol {
                TokenKind::Minus => function.name.name = Ustr::from("__unm"),
                TokenKind::Bang => function.name.name = Ustr::from("__not"),
                TokenKind::BitNot => {}
                _ => {
                    return Err(CompilerError::new_with_label(
                        sym_span,
                        format!(
                            "operator `{}` takes two parameters; only `-` and `!` have a prefix (unary) form",
                            symbol.operator_spelling()
                        ),
                    ));
                }
            }
        }
        Ok(ItemKind::Function(function))
    }

    fn finish_fn_def(
        &mut self,
        mut name: Ident,
        start: Span,
        is_macro: bool,
        mut is_coroutine: bool,
        extern_body: Option<FunctionBody>,
        backend: FnBackend,
    ) -> PResult<FunctionDef> {
        let mut generics = self.parse_generics()?;
        let qualified = self.kind() == TokenKind::DoubleColon
            && matches!(self.peek_kind(), TokenKind::Ident(_));
        let receiver = if qualified || self.kind() == TokenKind::Dot {
            if is_macro {
                return Err(self.err("the method form `Type.name` is for functions, not macros"));
            }
            if extern_body.is_some() {
                return Err(self.err(
                    "extern functions are foreign symbols and cannot declare a receiver path",
                ));
            }
            let path = if qualified {
                let path = self.parse_path_tail(name.clone())?;
                generics = self.parse_generics()?;
                if self.kind() != TokenKind::Dot {
                    return Err(self.err(
                        "a qualified receiver path must end in `.method` (`lib::Actor.heal`)",
                    ));
                }
                Some(path)
            } else {
                None
            };
            self.advance(); // `.`
            let method = self.expect_ident()?;
            let method_generics = self.parse_generics()?;
            if !qualified && self.kind() == TokenKind::Dot {
                return Err(self.err("the receiver path may have only one segment (`Type.name`)"));
            }
            for pg in &generics {
                if method_generics
                    .iter()
                    .any(|mg| mg.name.name == pg.name.name)
                {
                    return Err(self.err(format!(
                        "generic `{}` is declared in both the receiver path and the method generics",
                        pg.name.name
                    )));
                }
            }
            let receiver = if let Some(path) = path {
                let last = path.segments.last().expect("path has segments").clone();
                let ty = self.ast.ty(path.span(), TypeExprKind::Path(path), false);
                is_coroutine = false;
                ((last, 0), ty)
            } else {
                let ty = self.synth_receiver_type(&name, &generics);
                ((name, generics.len()), ty)
            };
            name = method;
            generics.extend(method_generics);
            Some(receiver)
        } else {
            None
        };
        let (receiver_surface, receiver_ty) = receiver.unzip();

        let (params, params_span) = self.parse_params(receiver_ty)?;
        self.skip_comments(); // `f() /* c */ -> T`
        let return_ty = if self.eat(TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };

        if matches!(extern_body, Some(FunctionBody::ExternLua(Some(_))))
            && matches!(self.kind(), TokenKind::OpenCurly | TokenKind::Assign)
        {
            return Err(self.err("an extern Lua symbol binding cannot have a body"));
        }

        let (body, macro_items) = if self.kind() == TokenKind::OpenCurly {
            if is_macro {
                let mut items = Vec::new();
                let block = self.parse_braced_block(Some(&mut items))?;
                (FunctionBody::Rock(block), items)
            } else {
                (FunctionBody::Rock(self.parse_block()?), Vec::new())
            }
        } else {
            let body = if self.eat(TokenKind::Assign) {
                self.skip_comments();
                let src = match self.kind() {
                    TokenKind::String(s) | TokenKind::RawString(s) => {
                        self.advance();
                        s
                    }
                    _ => {
                        return Err(self.err(
                            "an `= <body>` function body must be a string literal (an inline \
                             backend body); Rock has no expression-bodied fn form, use `{ .. }`",
                        ));
                    }
                };
                FunctionBody::Inline(backend, src)
            } else {
                let Some(eb) = extern_body else {
                    return Err(self.err(
                        "this function has no body: expected a block `{ .. }` or an inline \
                         backend body `= \"..\"` (only `extern fn` may omit one)",
                    ));
                };
                eb
            };
            self.eat(TokenKind::Semicolon);
            (body, Vec::new())
        };
        let span = self.span_from(start);
        // Expansion retags clones, so record parameter spans before it runs.
        if is_macro {
            let refs = crate::macro_expand::index_macro_param_refs(
                &self.ast,
                &params,
                body.rock_block(),
                &macro_items,
            );
            self.ast.macro_param_refs.extend(refs);
        }
        Ok(FunctionDef {
            span,
            keyword_span: Some(start),
            name,
            generics,
            params,
            params_span,
            return_ty,
            body,
            is_macro,
            is_coroutine,
            is_synthetic: false,
            json_field_for: None,
            macro_items,
            receiver_surface,
        })
    }

    fn token_starts_macro_body_item(&self) -> bool {
        match self.kind() {
            TokenKind::At
            | TokenKind::Fn
            | TokenKind::Extern
            | TokenKind::Struct
            | TokenKind::Enum
            | TokenKind::Newtype => true,
            TokenKind::Ident(name) if matches!(name.as_str(), "use" | "mod") => {
                matches!(self.peek_kind(), TokenKind::Ident(_))
            }
            TokenKind::Ident(name) if name.as_str() == "co" => {
                matches!(self.peek_kind(), TokenKind::Fn)
            }
            TokenKind::Ident(name) if name.as_str() == "value" => {
                matches!(self.peek_kind(), TokenKind::Ident(_))
            }
            TokenKind::Ident(name) if name.as_str() == "nobox" => {
                matches!(self.peek_kind(), TokenKind::Ident(v) if v.as_str() == "value")
            }
            TokenKind::Ident(name) if name.as_str() == "inline" => {
                self.peek_kind() == TokenKind::OpenParen
            }
            _ => false,
        }
    }

    fn parse_generics(&mut self) -> PResult<Vec<Generic>> {
        if self.kind() != TokenKind::LessThan {
            return Ok(Vec::new());
        }
        self.advance(); // `<`
        let out = self.parse_comma_list(TokenKind::GreaterThan, |p| {
            let name = p.expect_ident()?;
            let bound = if p.eat(TokenKind::Colon) {
                Some(p.parse_type()?)
            } else {
                None
            };
            let default = if p.eat(TokenKind::Assign) {
                Some(p.parse_type()?)
            } else {
                None
            };
            Ok(Generic {
                name,
                bound,
                default,
            })
        })?;
        self.expect_generic_close()?;
        Ok(out)
    }

    fn synth_receiver_type(
        &mut self,
        ident: &Ident,
        path_generics: &[Generic],
    ) -> Idx<TypeExprData> {
        if path_generics.is_empty() {
            let kind = primitive_type(ident.name.as_str())
                .unwrap_or_else(|| TypeExprKind::Ident(ident.clone()));
            return self.ast.ty(ident.span, kind, false);
        }
        let mut args = Vec::with_capacity(path_generics.len());
        for g in path_generics {
            args.push(
                self.ast
                    .ty(g.name.span, TypeExprKind::Ident(g.name.clone()), false),
            );
        }
        self.ast.ty(
            ident.span,
            TypeExprKind::App {
                ctor: ident.clone(),
                args,
            },
            false,
        )
    }

    fn parse_params(
        &mut self,
        receiver_ty: Option<Idx<TypeExprData>>,
    ) -> PResult<(Vec<Param>, Span)> {
        let open = self.span();
        self.expect(TokenKind::OpenParen)?;
        let mut params = Vec::new();
        if let Some(ty) = receiver_ty {
            self.skip_comments();
            if self.kind() == TokenKind::CloseParen {
                return Err(self.err("the method form requires a receiver parameter"));
            }
            let name = self.expect_ident()?;
            if self.kind() == TokenKind::Colon {
                return Err(self.err(
                    "the receiver takes its type from the path; write it without a type annotation",
                ));
            }
            let span = name.span;
            params.push(Param {
                span,
                name,
                ty,
                default: None,
                is_variadic: false,
                annotations: Vec::new(),
            });
            self.skip_comments();
            self.eat(TokenKind::Comma);
        }
        params.extend(self.parse_comma_list(TokenKind::CloseParen, |p| {
            let annotations = p.parse_item_annotations()?;
            let name = p.expect_ident()?;
            p.expect(TokenKind::Colon)?;
            let leading_variadic = p.eat(TokenKind::TripleDot);
            let ty = p.parse_type()?;
            let is_variadic = leading_variadic || p.eat(TokenKind::TripleDot);
            let default = if p.eat(TokenKind::Assign) {
                Some(p.with_allow_literals(true, |p| p.parse_expr(0))?)
            } else {
                None
            };
            let span = p.span_from(name.span);
            Ok(Param {
                span,
                name,
                ty,
                default,
                is_variadic,
                annotations,
            })
        })?);
        self.skip_comments();
        self.expect(TokenKind::CloseParen)?;
        Ok((params, self.span_from(open)))
    }

    fn parse_struct(&mut self, is_value: bool, is_nobox: bool) -> PResult<ItemKind> {
        let start = self.span();
        if is_nobox {
            self.advance();
            match self.kind() {
                TokenKind::Ident(n) if n.as_str() == "value" => self.advance(),
                _ => return Err(self.err("`nobox` requires `value`")),
            }
        } else if is_value {
            self.advance();
        } else {
            self.expect(TokenKind::Struct)?;
        }
        let name = self.expect_ident()?;
        let generics = self.parse_generics()?;
        let fields = self.parse_field_block()?;
        let span = self.span_from(start);
        Ok(ItemKind::Struct(StructDef {
            span,
            keyword_span: Some(start),
            name,
            generics,
            fields,
            is_value,
            is_nobox,
        }))
    }

    fn parse_field_block(&mut self) -> PResult<Vec<Field>> {
        self.expect(TokenKind::OpenCurly)?;
        self.brace_depth += 1;
        let mut fields = Vec::new();
        loop {
            self.skip_comments();
            if self.kind() == TokenKind::CloseCurly || self.at_end() {
                break;
            }
            let mods = self.parse_vis_mods()?;
            let (vis, set_private) = (mods.vis, mods.set_private);
            let annotations = self.parse_item_annotations()?;
            // `embed` is contextual: it is the modifier only when a field name
            // follows it, so `embed: int` stays a field named `embed`.
            let is_embed = matches!(self.kind(), TokenKind::Ident(n) if n.as_str() == "embed")
                && matches!(self.peek_kind(), TokenKind::Ident(_));
            if is_embed {
                self.advance();
            }
            let name = self.expect_ident()?;
            self.expect(TokenKind::Colon)?;
            let ty = self.parse_type()?;
            let default = if self.eat(TokenKind::Assign) {
                Some(self.with_allow_literals(true, |p| p.parse_expr(0))?)
            } else {
                None
            };
            let span = self.span_from(name.span);
            fields.push(Field {
                span,
                name,
                ty,
                vis,
                set_private,
                default,
                annotations,
                is_embed,
            });
            self.skip_comments();
            self.eat(TokenKind::Comma);
        }
        self.brace_depth -= 1;
        self.expect(TokenKind::CloseCurly)?;
        Ok(fields)
    }

    fn parse_enum(&mut self) -> PResult<ItemKind> {
        let start = self.span();
        self.expect(TokenKind::Enum)?;
        let name = self.expect_ident()?;
        let generics = self.parse_generics()?;
        self.expect(TokenKind::OpenCurly)?;
        self.delimiter_depth += 1;
        let mut variants = Vec::new();
        loop {
            self.skip_comments();
            if self.kind() == TokenKind::CloseCurly || self.at_end() {
                break;
            }
            let vname = self.expect_ident()?;
            let kind = if self.kind() == TokenKind::OpenParen {
                self.advance();
                self.delimiter_depth += 1;
                let tys = self.parse_comma_list(TokenKind::CloseParen, |p| {
                    if matches!(p.kind(), TokenKind::Ident(_)) && p.peek_kind() == TokenKind::Colon
                    {
                        p.advance(); // field name
                        p.advance(); // `:`
                    }
                    p.parse_type()
                })?;
                self.delimiter_depth -= 1;
                self.expect(TokenKind::CloseParen)?;
                EnumVariantKind::Tuple(tys)
            } else if self.kind() == TokenKind::OpenCurly {
                EnumVariantKind::Record(self.parse_field_block()?)
            } else {
                EnumVariantKind::Unit
            };
            let span = self.span_from(vname.span);
            variants.push(EnumVariant {
                span,
                name: vname,
                kind,
            });
            self.skip_comments();
            self.eat(TokenKind::Comma);
        }
        self.delimiter_depth -= 1;
        self.expect(TokenKind::CloseCurly)?;
        let span = self.span_from(start);
        Ok(ItemKind::Enum(EnumDef {
            span,
            keyword_span: Some(start),
            name,
            generics,
            variants,
        }))
    }

    fn parse_newtype(&mut self) -> PResult<ItemKind> {
        let start = self.span();
        self.expect(TokenKind::Newtype)?;
        let name = self.expect_ident()?;
        let generics = self.parse_generics()?;
        self.expect(TokenKind::OpenParen)?;
        let wrapped = self.parse_type()?;
        if self.kind() == TokenKind::Comma {
            return Err(self.err(format!(
                "a newtype wraps exactly one type, but `{}` declares more than one \
                 (use a struct, or wrap a tuple: `newtype {}((A, B))`)",
                name.name, name.name
            )));
        }
        self.expect(TokenKind::CloseParen)?;
        let span = self.span_from(start);
        Ok(ItemKind::Newtype(NewtypeDef {
            span,
            name,
            generics,
            wrapped,
        }))
    }

    fn parse_decorator(&mut self) -> PResult<ItemKind> {
        let start = self.span();
        self.expect(TokenKind::Deco)?;
        self.skip_comments();
        if matches!(self.kind(), TokenKind::Ident(n) if n.as_str() == "init") {
            self.advance();
        }
        let name = self.expect_ident()?;
        let params = if self.kind() == TokenKind::OpenParen {
            self.parse_params(None)?.0
        } else {
            Vec::new()
        };
        let body = self.parse_block()?;
        let span = self.span_from(start);
        Ok(ItemKind::Decorator(DecoratorDef {
            span,
            name,
            params,
            body,
        }))
    }

    fn parse_global(&mut self, is_const: bool) -> PResult<ItemKind> {
        let start = self.span();
        self.advance(); // `global` / `const`
        let name = self.expect_ident()?;
        let ty = if self.eat(TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let value = if self.eat(TokenKind::Assign) {
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        let span = self.span_from(start);
        Ok(ItemKind::Global(GlobalDef {
            span,
            keyword_span: Some(start),
            name,
            ty,
            value,
            is_const,
        }))
    }

    fn parse_compiler_directive(&mut self) -> ItemKind {
        let tok = self.kind();
        self.advance();
        let directive = match tok {
            TokenKind::DirectiveIf => CompilerDirective::If(self.directive_condition()),
            TokenKind::DirectiveElseIf => CompilerDirective::ElseIf(self.directive_condition()),
            TokenKind::DirectiveElse => CompilerDirective::Else,
            _ => CompilerDirective::End,
        };
        ItemKind::CompilerDirective(directive)
    }

    fn directive_condition(&mut self) -> Ustr {
        if let TokenKind::Ident(c) = self.kind() {
            self.advance();
            c
        } else {
            Ustr::from("")
        }
    }

    fn parse_path_tail(&mut self, first: Ident) -> PResult<Path> {
        self.finish_path(first, false)
    }

    fn finish_path(&mut self, first: Ident, from_root: bool) -> PResult<Path> {
        let mut segments = vec![first];
        while self.eat(TokenKind::DoubleColon) {
            segments.push(self.expect_ident()?);
        }
        Ok(Path {
            from_root,
            segments,
        })
    }

    fn finish_type_path(&mut self, path: Path) -> PResult<TypeExprKind> {
        if self.kind() == TokenKind::LessThan {
            self.advance();
            let args = self.parse_type_arg_list()?;
            return Ok(TypeExprKind::AppPath { ctor: path, args });
        }
        Ok(TypeExprKind::Path(path))
    }

    fn parse_type_arg_list(&mut self) -> PResult<Vec<Idx<TypeExprData>>> {
        let args = self.parse_comma_list(TokenKind::GreaterThan, |p| p.parse_type())?;
        self.expect_generic_close()?;
        Ok(args)
    }

    fn parse_root_path(&mut self) -> PResult<Path> {
        self.advance(); // leading `::`
        let first = self.expect_ident()?;
        self.finish_path(first, true)
    }

    fn parse_type(&mut self) -> PResult<Idx<TypeExprData>> {
        self.skip_comments();
        let start = self.span();
        let mut nullable = false;

        let kind = match self.kind() {
            TokenKind::OpenSquare => {
                self.advance();
                let inner = self.parse_type()?;
                self.expect(TokenKind::CloseSquare)?;
                TypeExprKind::Array(inner)
            }
            TokenKind::DoubleColon => {
                let path = self.parse_root_path()?;
                self.finish_type_path(path)?
            }
            TokenKind::OpenParen => {
                self.advance();
                let mut ts = Vec::new();
                let mut trailing_comma = false;
                while self.kind() != TokenKind::CloseParen && !self.at_end() {
                    ts.push(self.parse_type()?);
                    self.skip_comments(); // `(int /* c */, string)`
                    if self.eat(TokenKind::Comma) {
                        trailing_comma = true;
                    } else {
                        trailing_comma = false;
                        break;
                    }
                }
                self.expect(TokenKind::CloseParen)?;
                if self.eat(TokenKind::Arrow) {
                    let ret = self.parse_type()?;
                    TypeExprKind::Function { params: ts, ret }
                } else if ts.is_empty() {
                    TypeExprKind::Unit
                } else if ts.len() == 1 && !trailing_comma {
                    // `(T)` is parenthesized T, not a 1-tuple — reuse the inner
                    // type's kind, ORing in any outer `?`.
                    let inner = ts[0];
                    let data = &self.ast.types[inner];
                    nullable = data.is_nullable;
                    data.kind.clone()
                } else {
                    TypeExprKind::Tuple(ts)
                }
            }
            TokenKind::Fn => {
                self.advance();
                self.expect(TokenKind::OpenParen)?;
                let params =
                    self.parse_bare_comma_list(TokenKind::CloseParen, |p| p.parse_type())?;
                self.expect(TokenKind::CloseParen)?;
                let ret = if self.eat(TokenKind::Arrow) {
                    self.parse_type()?
                } else {
                    self.ast.ty(self.span(), TypeExprKind::Unit, false)
                };
                TypeExprKind::Function { params, ret }
            }
            TokenKind::Ident(name) => {
                let ident_span = self.span();
                self.advance();
                if let Some(kind) = primitive_type(name.as_str()) {
                    kind
                } else if name.as_str() == "ref" {
                    TypeExprKind::Ref(self.parse_type()?)
                } else {
                    let ctor = Ident {
                        span: ident_span,
                        name,
                    };
                    if self.kind() == TokenKind::DoubleColon {
                        let path = self.parse_path_tail(ctor)?;
                        self.finish_type_path(path)?
                    } else if self.kind() == TokenKind::LessThan {
                        self.advance();
                        let args = self.parse_type_arg_list()?;
                        TypeExprKind::App { ctor, args }
                    } else {
                        TypeExprKind::Ident(ctor)
                    }
                }
            }
            TokenKind::QuestionMark => {
                // `?` type hole (inferred). Note: a `?` AFTER a type is the
                // nullable suffix, handled below — this arm is the leading form.
                self.advance();
                let id = self.gen_hole_id();
                // Optional name immediately after `?` (`?foo`): in type position
                // `?` + ident is unambiguous (nullable is post-fix, `??` is a
                // value op), so no adjacency check is needed.
                let name = if let TokenKind::Ident(n) = self.kind() {
                    self.advance();
                    Some(n)
                } else {
                    None
                };
                TypeExprKind::Hole(id, name)
            }
            other => return Err(self.err(format!("unsupported type: {:?}", other))),
        };

        if self.eat(TokenKind::QuestionMark) {
            nullable = true;
        }
        let span = self.span_from(start);
        let base = self.ast.ty(span, kind, nullable);
        if self.eat(TokenKind::Arrow) {
            let ret = self.parse_type()?;
            let fspan = self.span_from(start);
            return Ok(self.ast.ty(
                fspan,
                TypeExprKind::Function {
                    params: vec![base],
                    ret,
                },
                false,
            ));
        }
        Ok(base)
    }

    fn parse_block(&mut self) -> PResult<Idx<BlockData>> {
        self.nesting_level(|p| {
            p.skip_comments();
            p.parse_braced_block(None)
        })
    }

    /// Bound tree height as well as recursion depth: a Pratt loop can build
    /// a deep left-associative tree without a deep parser call stack.
    fn nesting_level<T>(&mut self, parse: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        if self.nesting >= MAX_NESTING_DEPTH {
            return Err(self.too_deep_error(self.span()));
        }
        let start = self.span();
        self.nesting += 1;
        let enclosing = std::mem::take(&mut self.nested_height);
        let result = parse(self);
        let height = self.nested_height + 1;
        self.nesting -= 1;
        self.nested_height = enclosing.max(height);
        match result {
            Ok(_) if height > MAX_NESTING_DEPTH => Err(self.too_deep_error(start)),
            result => result,
        }
    }

    fn too_deep_error(&mut self, span: Span) -> CompilerError {
        self.too_deep = true;
        CompilerError::new_with_label(
            span,
            format!(
                "source nested too deeply: more than {MAX_NESTING_DEPTH} levels of expressions and blocks"
            ),
        )
    }

    fn parse_braced_block(
        &mut self,
        mut macro_items: Option<&mut Vec<Item>>,
    ) -> PResult<Idx<BlockData>> {
        let start = self.expect(TokenKind::OpenCurly)?;
        self.brace_depth += 1;
        let mut statements = Vec::new();
        loop {
            while self.eat(TokenKind::Semicolon) {}
            // A comment on its own line is a standalone `(comment ...)`
            // statement (rock's parse_block does the same); a comment on the
            // same line as the previous statement was already consumed as a
            // trailing attachment below.
            while let TokenKind::Comment(text) = self.kind() {
                let span = self.span();
                statements.push(self.ast.stmt(span, StatementKind::Comment(text)));
                self.advance();
            }
            if self.kind() == TokenKind::CloseCurly || self.at_end() {
                break;
            }
            if let Some(items) = &mut macro_items
                && self.token_starts_macro_body_item()
            {
                items.push(self.parse_item()?);
                continue;
            }
            let before = self.pos;
            match self.parse_statement() {
                Ok(stmt) => {
                    statements.push(stmt);
                    self.consume_trailing_comments();
                    while self.eat(TokenKind::Semicolon) {}
                }
                Err(e) if macro_items.is_some() => return Err(e),
                Err(e) if self.too_deep => {
                    self.brace_depth -= 1;
                    return Err(e);
                }
                Err(e) => {
                    // Recover this statement without losing the surrounding scope.
                    self.ast.recovery_errors.push(e);
                    // Guarantee forward progress: if the failed parse consumed
                    // nothing, drop one token so recovery can't spin.
                    if self.pos == before {
                        self.advance();
                    }
                    if !self.recover_to_next_statement() {
                        break;
                    }
                }
            }
        }
        self.brace_depth -= 1;
        let close = self.expect(TokenKind::CloseCurly)?;
        let span = self.span_from(start); // through the closing `}`
        Ok(self.ast.block(span, Some((start, close)), statements))
    }

    fn consume_trailing_comments(&mut self) {
        if self.pos == 0 {
            return;
        }
        let stmt_line = self.tokens[self.pos - 1].span.line_range().0;
        while matches!(self.kind(), TokenKind::Comment(_))
            && self.span().line_range().0 == stmt_line
        {
            self.advance();
        }
    }

    fn parse_statement(&mut self) -> PResult<Idx<StatementData>> {
        let start = self.span();

        if let TokenKind::Ident(name) = self.kind() {
            if matches!(name.as_str(), "on_resume" | "on_killed" | "on_hot_reload")
                && self.peek_kind() == TokenKind::OpenCurly
            {
                self.advance(); // hook keyword
                let body = self.parse_block()?;
                let span = self.span_from(start);
                return Ok(self.ast.stmt(
                    span,
                    StatementKind::LifecycleHook {
                        keyword_span: Some(start),
                        name,
                        body,
                    },
                ));
            }
        }

        if self.at_use_decl() {
            let decl = self.parse_use_decl(start, None)?;
            return Ok(self.ast.stmt(decl.span, StatementKind::Use(decl)));
        }

        // Contextual `static <name> ...` local. Only fires when `static` heads a
        // statement and is followed by an identifier (the binding name), so
        // `static` stays usable as an ordinary ident (`load_audio(p, static)`).
        if matches!(self.kind(), TokenKind::Ident(n) if n.as_str() == "static")
            && matches!(self.peek_kind(), TokenKind::Ident(_) | TokenKind::OpenParen)
        {
            self.advance(); // `static`
            if self.kind() == TokenKind::OpenParen {
                return Err(self.err(STATIC_PATTERN_ERROR));
            }
            let name = self.expect_ident()?;
            if matches!(self.kind(), TokenKind::OpenCurly | TokenKind::OpenParen) {
                return Err(self.err(STATIC_PATTERN_ERROR));
            }
            let ty = if self.eat(TokenKind::Colon) {
                Some(self.parse_type()?)
            } else {
                None
            };
            self.expect(TokenKind::Assign)?;
            let value = self.parse_expr(0)?;
            let span = self.span_from(start);
            return Ok(self.ast.stmt(
                span,
                StatementKind::Static(Static {
                    span,
                    keyword_span: Some(start),
                    name,
                    ty,
                    value,
                }),
            ));
        }

        if self.kind() == TokenKind::At
            && matches!(self.peek_kind(), TokenKind::Ident(_))
            && self.tokens.get(self.pos + 2).map(|t| t.kind) == Some(TokenKind::Fn)
        {
            return Err(self.err("nested functions cannot have annotations (v1)"));
        }
        if self.kind() == TokenKind::Fn {
            return self.parse_nested_fn(start);
        }
        if self.kind() == TokenKind::At {
            if let TokenKind::Ident(name) = self.peek_kind() {
                return Err(self.err(format!("unknown annotation `@{name}` on a statement")));
            }
        }

        match self.kind() {
            TokenKind::Let => {
                self.advance();
                let pattern = self.parse_pattern()?;
                let ty = if self.eat(TokenKind::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                let value = if self.eat(TokenKind::Assign) {
                    Some(self.parse_expr(0)?)
                } else {
                    None
                };
                let else_block = if value.is_some() && self.kind() == TokenKind::Else {
                    self.advance();
                    Some(self.parse_block()?)
                } else {
                    None
                };
                let span = self.span_from(start);
                Ok(self.ast.stmt(
                    span,
                    StatementKind::Let(Let {
                        span,
                        keyword_span: Some(start),
                        pattern,
                        ty,
                        value,
                        else_block,
                    }),
                ))
            }
            TokenKind::Return => {
                self.advance();
                let value = if self.stmt_terminator() {
                    None
                } else {
                    Some(self.parse_expr(0)?)
                };
                let span = self.span_from(start);
                Ok(self.ast.stmt(
                    span,
                    StatementKind::Return {
                        keyword_span: Some(start),
                        value,
                    },
                ))
            }
            TokenKind::Break => {
                self.advance();
                Ok(self.ast.stmt(
                    start,
                    StatementKind::Break {
                        keyword_span: Some(start),
                    },
                ))
            }
            TokenKind::Continue => {
                self.advance();
                Ok(self.ast.stmt(
                    start,
                    StatementKind::Continue {
                        keyword_span: Some(start),
                    },
                ))
            }
            TokenKind::For => {
                self.advance();
                self.skip_comments(); // `for /* c */ v in /* c */ arr`
                let pattern = self.parse_pattern()?;
                let ty = if self.eat(TokenKind::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                self.skip_comments();
                self.expect(TokenKind::In)?;
                self.skip_comments();
                let iter = self.with_allow_literals(false, |p| p.parse_expr(0))?;
                let body = self.parse_block()?;
                let span = self.span_from(start);
                Ok(self.ast.stmt(
                    span,
                    StatementKind::For(For {
                        span,
                        keyword_span: Some(start),
                        pattern,
                        ty,
                        iter,
                        body,
                    }),
                ))
            }
            TokenKind::While => {
                self.advance();
                self.skip_comments_in_expr();
                if self.kind() == TokenKind::Let {
                    self.advance();
                    let pattern = self.parse_pattern()?;
                    self.expect(TokenKind::Assign)?;
                    let value = self.with_allow_literals(false, |p| p.parse_expr(16))?;
                    let body = self.parse_block()?;
                    let span = self.span_from(start);
                    return Ok(self.ast.stmt(
                        span,
                        StatementKind::WhileLet(WhileLet {
                            span,
                            keyword_span: Some(start),
                            pattern,
                            value,
                            body,
                        }),
                    ));
                }
                let condition = self.with_allow_literals(false, |p| p.parse_expr(0))?;
                let body = self.parse_block()?;
                let span = self.span_from(start);
                Ok(self.ast.stmt(
                    span,
                    StatementKind::While(While {
                        span,
                        keyword_span: Some(start),
                        condition,
                        body,
                    }),
                ))
            }
            TokenKind::Loop => {
                self.advance();
                let condition = self.ast.expr(start, ExprKind::Bool(true));
                let body = self.parse_block()?;
                let span = self.span_from(start);
                Ok(self.ast.stmt(
                    span,
                    StatementKind::While(While {
                        span,
                        keyword_span: Some(start),
                        condition,
                        body,
                    }),
                ))
            }
            TokenKind::Defer => {
                self.advance();
                let kind = if self.kind() == TokenKind::OpenCurly {
                    DeferKind::Block(self.parse_block()?)
                } else {
                    DeferKind::Expression(self.parse_expr(0)?)
                };
                let span = self.span_from(start);
                Ok(self.ast.stmt(span, StatementKind::Defer(kind)))
            }
            _ => {
                let e = self.parse_expr(0)?;
                let span = self.span_from(start);
                Ok(self.ast.stmt(span, StatementKind::Expression(e)))
            }
        }
    }

    fn parse_nested_fn(&mut self, start: Span) -> PResult<Idx<StatementData>> {
        self.expect(TokenKind::Fn)?;
        let name = self.expect_ident()?;
        if self.kind() == TokenKind::LessThan {
            return Err(self.err("nested functions cannot be generic (v1)"));
        }
        let (params, params_span) = self.parse_params(None)?;
        self.skip_comments();
        let return_ty = if self.eat(TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.skip_comments();
        if self.kind() != TokenKind::OpenCurly {
            return Err(self.err("a nested function needs a `{ }` body"));
        }
        let body = self.parse_block()?;
        let span = self.span_from(start);
        Ok(self.ast.stmt(
            span,
            StatementKind::NestedFn(NestedFn {
                span,
                keyword_span: Some(start),
                name,
                params,
                params_span,
                return_ty,
                body,
            }),
        ))
    }

    fn stmt_terminator(&self) -> bool {
        matches!(
            self.kind(),
            TokenKind::CloseCurly | TokenKind::Semicolon | TokenKind::EOF | TokenKind::Comment(_)
        ) || self.at_end()
            || self.prev_trailing_newlines() > 0
    }

    fn parse_expr(&mut self, min_bp: u8) -> PResult<Idx<ExprData>> {
        self.nesting_level(|p| p.parse_expr_level(min_bp))
    }

    fn parse_expr_level(&mut self, min_bp: u8) -> PResult<Idx<ExprData>> {
        let mut left = self.parse_prefix()?;
        let mut measured = left;

        loop {
            if left != measured {
                measured = left;
                self.nested_height += 1;
            }
            self.skip_comments_in_expr();
            let op = self.kind();

            // Inside a paren group's head the `:` belongs to the
            // `(expr : Type)` ascription form owned by the paren arm, so stop
            // here instead of consuming it as a binop.
            if op == TokenKind::Colon && self.paren_head > 0 {
                break;
            }

            if op == TokenKind::Bang && self.peek_kind() == TokenKind::OpenParen {
                let callee = match &self.ast.exprs[left].kind {
                    ExprKind::Ident(_) => Some((left, None)),
                    ExprKind::FieldAccess(receiver, method) => {
                        let receiver = *receiver;
                        let func = self.ast.expr(method.span, ExprKind::Ident(method.clone()));
                        Some((func, Some(receiver)))
                    }
                    _ => None,
                };
                if let Some((func, receiver)) = callee {
                    self.advance(); // `!`
                    left = self.parse_sigil_call(func, receiver)?;
                    continue;
                }
            }

            if op == TokenKind::LessThan
                && let Some(generic_args) = self.try_generic_call_args()
            {
                left = self.parse_call(left, generic_args)?;
                continue;
            }

            let power = op.binding_power();
            if let Some(BindingPower::Postfix(l_bp)) = power {
                if l_bp < min_bp {
                    break;
                }
                if op_starts_expr(op) && self.newline_breaks() {
                    break;
                }
                match op {
                    TokenKind::OpenParen => {
                        left = self.parse_call(left, Vec::new())?;
                        continue;
                    }
                    TokenKind::OpenSquare => {
                        self.advance();
                        self.delimiter_depth += 1;
                        let idx = self.with_allow_literals(true, |p| p.parse_expr(0))?;
                        self.delimiter_depth -= 1;
                        let end = self.expect(TokenKind::CloseSquare)?;
                        let span = self.node_span(left, end);
                        left = self.ast.expr(span, ExprKind::Index(left, idx));
                        continue;
                    }
                    TokenKind::QuestionMark | TokenKind::PlusPlus | TokenKind::MinusMinus => {
                        let span = self.node_span(left, self.span());
                        self.advance();
                        let kind = match op {
                            TokenKind::QuestionMark => ExprKind::QuestionMark(left),
                            TokenKind::PlusPlus => ExprKind::PostIncrement(left),
                            _ => ExprKind::PostDecrement(left),
                        };
                        left = self.ast.expr(span, kind);
                        continue;
                    }
                    TokenKind::QuestionDot => {
                        self.advance();
                        if self.kind() == TokenKind::Let {
                            self.advance();
                            let binding = match &self.ast.exprs[left].kind {
                                ExprKind::Ident(id) => id.clone(),
                                _ => Ident {
                                    span: self.ast.exprs[left].span,
                                    name: Ustr::from("value"),
                                },
                            };
                            let body = self.parse_block()?;
                            let span = self.node_span(left, self.prev_span());
                            left = self.ast.expr(
                                span,
                                ExprKind::SafeNavigationLet {
                                    receiver: left,
                                    binding,
                                    body,
                                },
                            );
                            continue;
                        }
                        let Some(field) = self.eat_field_name() else {
                            break;
                        };
                        let span = self.node_span(left, field.span);
                        left = self.ast.expr(span, ExprKind::SafeNavigation(left, field));
                        continue;
                    }
                    TokenKind::At => {
                        left = self.parse_at(left)?;
                        continue;
                    }
                    _ => break,
                }
            }

            if let Some(BindingPower::Infix(l_bp, r_bp)) = power {
                if l_bp < min_bp {
                    break;
                }
                if op == TokenKind::Dot {
                    left = self.parse_dot(left)?;
                    continue;
                }

                if op == TokenKind::As {
                    self.advance();
                    let ty = self.parse_type()?;
                    let span = self.node_span(left, self.prev_span());
                    left = self.ast.expr(span, ExprKind::As(left, ty));
                    continue;
                }

                let op_span = self.span();
                self.advance();
                let right = self.parse_expr(r_bp)?;
                left = self.build_infix(left, op, op_span, right);
                continue;
            }

            // No-parens call `foo a b` -> `foo(a, b)`. Reached only when `op`
            // is a value-start token (no infix/postfix binding power), so it
            // never competes with operator handling above.
            if let Some(call) = self.try_no_parens_call(left, min_bp)? {
                left = call;
                continue;
            }

            break;
        }

        Ok(left)
    }

    fn try_no_parens_call(
        &mut self,
        func: Idx<ExprData>,
        min_bp: u8,
    ) -> PResult<Option<Idx<ExprData>>> {
        if min_bp >= 50 || self.in_no_parens_args {
            return Ok(None);
        }
        if !matches!(
            self.ast.exprs[func].kind,
            ExprKind::Ident(_) | ExprKind::Paren(_)
        ) {
            return Ok(None);
        }
        if self.prev_trailing_newlines() > 0 || !no_parens_arg_start(self.kind()) {
            return Ok(None);
        }

        self.in_no_parens_args = true;
        let mut args = Vec::new();
        loop {
            args.push(FunctionCallArg::positional(self.parse_expr(15)?));

            if !no_parens_arg_continue(self.kind())
                || self.prev_trailing_newlines() > 0
                || self.delimiter_depth > 0
            {
                break;
            }
        }
        self.in_no_parens_args = false;

        let span = self.node_span(func, self.prev_span());
        Ok(Some(self.ast.expr(
            span,
            ExprKind::FunctionCall {
                func,
                generic_args: Vec::new(),
                args,
                syntax: CallSyntax::NoParens,
            },
        )))
    }

    fn newline_breaks(&self) -> bool {
        self.prev_trailing_newlines() > 0 && (self.delimiter_depth == 0 || self.brace_depth > 0)
    }

    fn build_infix(
        &mut self,
        left: Idx<ExprData>,
        op: TokenKind,
        op_span: Span,
        right: Idx<ExprData>,
    ) -> Idx<ExprData> {
        let span = self.node_span(left, self.ast.exprs[right].span);
        if op == TokenKind::Assign {
            return self.ast.expr(
                span,
                ExprKind::Assign {
                    target: left,
                    value: right,
                },
            );
        }
        if op == TokenKind::Dollar {
            return self.ast.expr(
                span,
                ExprKind::FunctionCall {
                    func: left,
                    generic_args: Vec::new(),
                    args: vec![FunctionCallArg::positional(right)],
                    syntax: CallSyntax::Normal,
                },
            );
        }
        self.ast.expr(
            span,
            ExprKind::BinOp {
                left,
                op,
                op_span,
                right,
            },
        )
    }

    fn parse_dot(&mut self, left: Idx<ExprData>) -> PResult<Idx<ExprData>> {
        self.advance(); // consume `.`
        self.skip_comments();
        let field = if let Some(field) = self.eat_field_name() {
            field
        } else if let Some(kw) = self.kind().as_keyword_identifier() {
            let span = self.span();
            self.advance();
            Ident {
                span,
                name: Ustr::from(kw),
            }
        } else {
            return Err(self.err("field access `.` must be followed by an identifier or integer"));
        };
        let span = self.node_span(left, field.span);
        Ok(self.ast.expr(span, ExprKind::FieldAccess(left, field)))
    }

    fn eat_field_name(&mut self) -> Option<Ident> {
        let span = self.span();
        let name = match self.kind() {
            TokenKind::Ident(name) => name,
            TokenKind::Int(num, text) if is_tuple_index(&text) => {
                Ustr::from(&(num as usize).to_string())
            }
            _ => return None,
        };
        self.advance();
        Some(Ident { span, name })
    }

    fn try_generic_call_args(&mut self) -> Option<Vec<Idx<TypeExprData>>> {
        let saved = self.pos;
        self.advance(); // `<`
        let args = self.parse_bare_comma_list(TokenKind::GreaterThan, Self::parse_type);
        if let Ok(args) = args
            && self.expect_generic_close().is_ok()
            && self.kind() == TokenKind::OpenParen
        {
            return Some(args);
        }
        self.pos = saved;
        None
    }

    fn parse_call(
        &mut self,
        func: Idx<ExprData>,
        generic_args: Vec<Idx<TypeExprData>>,
    ) -> PResult<Idx<ExprData>> {
        self.advance(); // consume `(`
        self.delimiter_depth += 1;
        let mut seen_named = false;
        let args = self.parse_comma_list(TokenKind::CloseParen, |p| {
            let is_spread = p.eat(TokenKind::DoubleDot);
            let name = if is_spread {
                None
            } else if matches!(p.kind(), TokenKind::Ident(_)) && p.peek_kind() == TokenKind::Colon {
                let ident = p.expect_ident()?;
                p.expect(TokenKind::Colon)?;
                seen_named = true;
                Some(ident)
            } else if seen_named {
                return Err(p.err("positional argument after named argument is not allowed"));
            } else {
                None
            };
            let is_splat = !is_spread && p.eat(TokenKind::TripleDot);
            let value = p.with_allow_literals(true, |p| p.parse_expr(0))?;
            Ok(FunctionCallArg {
                name,
                value,
                is_splat,
                is_spread,
            })
        })?;
        self.delimiter_depth -= 1;
        let end = self.expect(TokenKind::CloseParen)?;
        let span = self.node_span(func, end);
        Ok(self.ast.expr(
            span,
            ExprKind::FunctionCall {
                func,
                generic_args,
                args,
                syntax: CallSyntax::Normal,
            },
        ))
    }

    fn parse_at(&mut self, left: Idx<ExprData>) -> PResult<Idx<ExprData>> {
        self.advance(); // consume `@`

        let body = if self.kind() == TokenKind::OpenCurly {
            self.parse_block()?
        } else {
            let rhs = self.parse_expr(10)?;
            let span = self.ast.exprs[rhs].span;
            let stmt = self.ast.stmt(span, StatementKind::Expression(rhs));
            self.ast.block(span, None, vec![stmt])
        };
        let body_span = self.ast.blocks[body].span;
        let lambda = self.ast.expr(
            body_span,
            ExprKind::Lambda {
                params: vec![],
                return_ty: None,
                body,
            },
        );
        let lambda_arg = FunctionCallArg::positional(lambda);

        if matches!(self.ast.exprs[left].kind, ExprKind::FunctionCall { .. }) {
            let new_span = self.node_span(left, body_span);
            if let ExprKind::FunctionCall { args, syntax, .. } = &mut self.ast.exprs[left].kind {
                args.push(lambda_arg);
                *syntax = match syntax {
                    CallSyntax::Macro => CallSyntax::AtMacro,
                    CallSyntax::MethodMacro => CallSyntax::AtMethodMacro,
                    _ => CallSyntax::At,
                };
            }
            self.ast.exprs[left].span = new_span;
            Ok(left)
        } else {
            let span = self.node_span(left, body_span);
            Ok(self.ast.expr(
                span,
                ExprKind::FunctionCall {
                    func: left,
                    generic_args: Vec::new(),
                    args: vec![lambda_arg],
                    syntax: CallSyntax::At,
                },
            ))
        }
    }

    fn parse_prefix(&mut self) -> PResult<Idx<ExprData>> {
        self.skip_comments();
        let start = self.span();

        if !self.arrow_lambda_blocked
            && self.kind() == TokenKind::OpenParen
            && self.looks_like_lambda_params()
        {
            self.advance(); // `(`
            let params = self.parse_comma_list(TokenKind::CloseParen, |p| {
                let name = p.expect_ident()?;
                let ty = if p.eat(TokenKind::Colon) {
                    Some(p.parse_type()?)
                } else {
                    None
                };
                let span = p.span_from(name.span);
                Ok(LambdaParam { span, name, ty })
            })?;
            self.expect(TokenKind::CloseParen)?;
            return self.finish_lambda(params, start);
        }

        if self.kind() == TokenKind::DoubleColon {
            let start = self.span();
            let path = self.parse_root_path()?;
            let span = self.span_from(start);
            return Ok(self.ast.expr(span, ExprKind::Path(path)));
        }

        if self.kind() == TokenKind::Dot && matches!(self.peek_kind(), TokenKind::Ident(_)) {
            self.advance(); // `.`
            let ident = self.expect_ident()?;
            let span = self.span_from(start);
            return Ok(self.ast.expr(span, ExprKind::SiblingFieldRef(ident)));
        }

        let kind = self.kind();

        if let Some(((), r_bp)) = kind.prefix_bp() {
            if matches!(
                kind,
                TokenKind::Minus
                    | TokenKind::Bang
                    | TokenKind::BitNot
                    | TokenKind::PlusPlus
                    | TokenKind::MinusMinus
            ) {
                self.advance();
                let operand = self.parse_expr(r_bp)?;
                let span = self.span_from(start);
                let k = match kind {
                    TokenKind::Minus => ExprKind::UnaryMinus(operand),
                    TokenKind::Bang => ExprKind::Not(operand),
                    TokenKind::BitNot => ExprKind::BitNot(operand),
                    TokenKind::PlusPlus => ExprKind::PreIncrement(operand),
                    TokenKind::MinusMinus => ExprKind::PreDecrement(operand),
                    _ => unreachable!(),
                };
                return Ok(self.ast.expr(span, k));
            }
        }

        if kind == TokenKind::TripleDot {
            self.advance();
            let operand = self.parse_expr(0)?;
            // The Pratt loop can consume a trailing comment before a closer.
            // Keep it outside the spread span so formatting emits it once.
            let span = self.merge_spans(start, self.ast.exprs[operand].span);
            return Ok(self.ast.expr(span, ExprKind::Spread(operand)));
        }

        if matches!(kind, TokenKind::BitAnd | TokenKind::BitXor) {
            self.advance();
            let name = self.expect_ident()?;
            let expr = if kind == TokenKind::BitAnd {
                ExprKind::FunctionRef(name)
            } else {
                ExprKind::AnaphoricIdent(name)
            };
            return Ok(self.ast.expr(self.span_from(start), expr));
        }

        // Only a byte-adjacent name belongs to the hole.
        if kind == TokenKind::QuestionMark {
            self.advance();
            let id = self.gen_hole_id();
            let name = if let TokenKind::Ident(n) = self.kind() {
                if self.prev_span().offset_range().1 == self.span().offset_range().0 {
                    self.advance();
                    Some(n)
                } else {
                    None
                }
            } else {
                None
            };
            let span = self.span_from(start);
            return Ok(self.ast.expr(span, ExprKind::Hole(id, name)));
        }

        match kind {
            TokenKind::String(s) => {
                self.advance();
                // Cooked text keeps interpolation holes and doubled braces.
                if self.holes.is_empty() && !s.as_str().contains(['{', '}']) {
                    Ok(self.ast.expr(start, ExprKind::String(s)))
                } else {
                    self.parse_interpolated(start)
                }
            }
            TokenKind::RawString(s) => {
                self.advance();
                Ok(self.ast.expr(start, ExprKind::String(s)))
            }
            TokenKind::Int(..)
            | TokenKind::Float(_)
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Null => Ok(self.plain_literal().expect("literal token")),
            TokenKind::Ident(name) => {
                self.advance();
                let base = Ident { span: start, name };
                if name.as_str() == "bitcast" && self.kind() == TokenKind::LessThan {
                    self.advance();
                    let target = self.parse_type()?;
                    self.expect_generic_close()?;
                    self.expect(TokenKind::OpenParen)?;
                    self.delimiter_depth += 1;
                    let value = self.with_allow_literals(true, |p| p.parse_expr(0))?;
                    self.delimiter_depth -= 1;
                    self.expect(TokenKind::CloseParen)?;
                    let span = self.span_from(start);
                    return Ok(self.ast.expr(span, ExprKind::Bitcast(value, target)));
                }
                if !self.arrow_lambda_blocked && self.kind() == TokenKind::FatArrow {
                    return self.finish_lambda(
                        vec![LambdaParam {
                            span: base.span,
                            name: base,
                            ty: None,
                        }],
                        start,
                    );
                }
                // `::` after an ident is ALWAYS a path - the resolver decides
                // whether the head names a module, type, or enum (no case heuristic).
                let qualified = self.kind() == TokenKind::DoubleColon;
                let path = if qualified {
                    self.parse_path_tail(base)?
                } else {
                    Path {
                        from_root: false,
                        segments: vec![base],
                    }
                };
                if self.struct_literal_follows() {
                    let (bases, fields) = self.parse_struct_fields()?;
                    if qualified && !bases.is_empty() {
                        return Err(self.err(
                            "functional-update `..base` is not supported on qualified record literals",
                        ));
                    }
                    let span = self.span_from(start);
                    return Ok(self.ast.expr(
                        span,
                        ExprKind::PathStructLiteral {
                            path,
                            bases,
                            fields,
                        },
                    ));
                }
                if qualified {
                    let span = self.span_from(start);
                    Ok(self.ast.expr(span, ExprKind::Path(path)))
                } else {
                    Ok(self
                        .ast
                        .expr(start, ExprKind::Ident(path.segments[0].clone())))
                }
            }
            TokenKind::Match => self.parse_match(),
            TokenKind::OpenParen => {
                self.advance();
                self.delimiter_depth += 1;
                if self.eat(TokenKind::CloseParen) {
                    self.delimiter_depth -= 1;
                    return Ok(self.ast.expr(self.span_from(start), ExprKind::Unit));
                }
                // The paren head stops Pratt parsing at the ascription colon.
                let first =
                    self.with_paren_head(|p| p.with_allow_literals(true, |p| p.parse_expr(0)))?;
                let kind = if self.eat(TokenKind::Colon) {
                    let ty = self.parse_type()?;
                    if self.kind() == TokenKind::Comma {
                        return Err(self.err(
                            "an ascription takes one expression: `(expr : Type)`; to ascribe a tuple write `((a, b) : (A, B))`",
                        ));
                    }
                    ExprKind::ParenAscription(first, ty)
                } else if self.kind() == TokenKind::Comma {
                    let mut items = vec![first];
                    while self.eat(TokenKind::Comma) {
                        self.skip_comments();
                        if self.kind() == TokenKind::CloseParen {
                            break;
                        }
                        items.push(self.with_paren_head(|p| {
                            p.with_allow_literals(true, |p| p.parse_expr(0))
                        })?);
                        // A colon after an element is the un-added bare
                        // ascription grammar, not a new typed-tuple form.
                        if self.kind() == TokenKind::Colon {
                            return Err(self.err(
                                "bare `expr : Type` is not grammar; parenthesize the ascription: `(expr : Type)`",
                            ));
                        }
                    }
                    ExprKind::Tuple(items)
                } else {
                    ExprKind::Paren(first)
                };
                self.delimiter_depth -= 1;
                let end = self.expect(TokenKind::CloseParen)?;
                Ok(self.ast.expr(self.merge_spans(start, end), kind))
            }
            TokenKind::OpenSquare => {
                self.advance();
                let items = self.parse_expr_list(TokenKind::CloseSquare)?;
                let end = self.expect(TokenKind::CloseSquare)?;
                let span = self.merge_spans(start, end);
                Ok(self.ast.expr(span, ExprKind::ArrayLiteral(items)))
            }
            TokenKind::OpenCurly => {
                let block = self.parse_block()?;
                let span = self.ast.blocks[block].span;
                Ok(self.ast.expr(span, ExprKind::Block(block)))
            }
            TokenKind::If => self.parse_if(),
            TokenKind::Break | TokenKind::Continue => {
                self.advance();
                let expr = if kind == TokenKind::Break {
                    ExprKind::BreakExpr
                } else {
                    ExprKind::ContinueExpr
                };
                Ok(self.ast.expr(start, expr))
            }
            TokenKind::Return | TokenKind::Yield => {
                self.advance();
                let value = if self.expr_terminates() {
                    None
                } else {
                    Some(self.parse_expr(0)?)
                };
                let expr = if kind == TokenKind::Yield {
                    ExprKind::Yield(value)
                } else {
                    ExprKind::ReturnExpr(value)
                };
                Ok(self.ast.expr(self.span_from(start), expr))
            }
            TokenKind::Await => {
                self.advance();
                let inner = self.parse_expr(0)?;
                Ok(self
                    .ast
                    .expr(self.span_from(start), ExprKind::YieldDelegate(inner)))
            }
            other => Err(self.err(format!("unexpected token in expression: {:?}", other))),
        }
    }

    fn expr_terminates(&self) -> bool {
        matches!(
            self.kind(),
            TokenKind::CloseParen
                | TokenKind::CloseCurly
                | TokenKind::CloseSquare
                | TokenKind::Comma
                | TokenKind::Semicolon
                | TokenKind::EOF
        ) || self.at_end()
            || self.prev_trailing_newlines() > 0
    }

    fn parse_expr_list(&mut self, close: TokenKind) -> PResult<Vec<Idx<ExprData>>> {
        self.delimiter_depth += 1;
        let items =
            self.parse_comma_list(close, |p| p.with_allow_literals(true, |p| p.parse_expr(0)))?;
        self.delimiter_depth -= 1;
        Ok(items)
    }

    fn parse_interpolated(&mut self, span: Span) -> PResult<Idx<ExprData>> {
        let file = span.file;
        let lit_start = span.offset_range().0;
        let nested = self.holes.iter().rev().find_map(|hole| {
            let local = hole.map.iter().position(|&offset| offset == lit_start)?;
            Some((Rc::clone(hole), local))
        });
        let base = self.source_base;
        let (text, map, mut pos) = if let Some((hole, local)) = &nested {
            (
                hole.text.as_str(),
                Some(hole.map.as_slice()),
                Pos {
                    at: *local,
                    line: hole.line,
                    column: hole.column,
                },
            )
        } else {
            let at = lit_start
                .checked_sub(base)
                .filter(|&at| at <= self.source.len())
                .ok_or_else(|| {
                    CompilerError::new_with_label(
                        span,
                        "string literal span does not point into the parsed source".to_string(),
                    )
                })?;
            (
                self.source,
                None,
                Pos {
                    at,
                    line: span.line_range().0,
                    column: span.col_range().0,
                },
            )
        };
        if interp::Flavor::at(text, pos.at).is_none() {
            return Err(CompilerError::new_with_label(
                span,
                "string literal span does not start a literal in the parsed source".to_string(),
            ));
        }
        let scan = interp::scan_string(text, pos.at);
        let mut parts = Vec::with_capacity(scan.pieces.len());
        for piece in &scan.pieces {
            pos.advance(text, piece.range().0);
            match piece {
                interp::Piece::Literal { text, .. } => {
                    parts.push(InterpolatedPart::Literal(Ustr::from(
                        &collapse_brace_escapes(text),
                    )));
                }
                interp::Piece::Hole { expr, spec, .. } => {
                    let cook = |at: (usize, usize)| {
                        let mut p = pos;
                        p.advance(text, at.0);
                        let (text, local) = scan.hole_source(text, at);
                        HoleFrame {
                            text,
                            map: local
                                .into_iter()
                                .map(|o| map.map_or(base + o, |m| m[o]))
                                .collect(),
                            line: p.line,
                            column: p.column,
                        }
                    };
                    let expr = cook(*expr);
                    let spec = spec.map(cook);
                    let e = self.in_hole_frame(expr, |p, f| {
                        p.parse_interp_subexpr(&f.text, file, &f.map, f.line, f.column)
                    })?;
                    let spec = match spec {
                        None => None,
                        Some(sf) => {
                            Some(self.in_hole_frame(sf, |p, f| p.parse_format_spec(f, file))?)
                        }
                    };
                    parts.push(InterpolatedPart::Expr(e, spec));
                }
            }
        }

        let kind = match parts.as_slice() {
            [] => ExprKind::String(Ustr::from("")),
            [InterpolatedPart::Literal(lit)] => ExprKind::String(*lit),
            _ => ExprKind::InterpolatedString(parts),
        };
        Ok(self.ast.expr(span, kind))
    }

    fn in_hole_frame<T>(
        &mut self,
        frame: HoleFrame,
        f: impl FnOnce(&mut Self, &HoleFrame) -> PResult<T>,
    ) -> PResult<T> {
        let frame = Rc::new(frame);
        self.holes.push(Rc::clone(&frame));
        let r = f(self, &frame);
        self.holes.pop();
        r
    }

    fn parse_interp_subexpr(
        &mut self,
        src: &str,
        file: Ustr,
        map: &[usize],
        line: usize,
        column: usize,
    ) -> PResult<Idx<ExprData>> {
        let mut tokens = Tokenizer::new_with_offset(src, file, map[0], line, column).tokenize()?;
        if map.windows(2).any(|w| w[1] > w[0] + 1) {
            let base = map[0];
            for t in &mut tokens {
                let (s, e) = t.span.offset_range();
                let remap = |o: usize| map.get(o.saturating_sub(base)).copied().unwrap_or(o);
                let (rs, re) = (remap(s), remap(e));
                t.span
                    .set_col_range((t.span.col_range().0 + (rs - s), t.span.col_range().1));
                t.span
                    .set_col_range((t.span.col_range().0, t.span.col_range().1 + (re - e)));
                t.span.set_offset_range((rs, re));
            }
        }
        let saved_tokens = std::mem::replace(&mut self.tokens, tokens);
        let saved_pos = std::mem::replace(&mut self.pos, 0);
        let saved_eof = self.eof_span;
        let saved_delim = self.delimiter_depth;
        let saved_brace = self.brace_depth;
        self.eof_span = self
            .tokens
            .last()
            .map(|t| t.span)
            .unwrap_or_else(Span::unknown);
        self.delimiter_depth = 0;
        self.brace_depth = 0;

        let result = self.parse_expr(0);

        self.tokens = saved_tokens;
        self.pos = saved_pos;
        self.eof_span = saved_eof;
        self.delimiter_depth = saved_delim;
        self.brace_depth = saved_brace;
        result
    }

    fn parse_format_spec(&mut self, frame: &HoleFrame, file: Ustr) -> PResult<FormatSpec> {
        let mut spec = FormatSpec::new();
        let format_str = &frame.text;
        let mut chars = format_str.chars().peekable();

        let is_align = |c: &char| matches!(c, '<' | '>' | '^' | '=');
        if chars.clone().nth(1).is_some_and(|c| is_align(&c)) {
            spec.fill = chars.next();
        }
        spec.alignment = chars.next_if(is_align);
        spec.sign = chars.next_if(|c| matches!(c, '+' | '-' | ' '));
        spec.alternate = chars.next_if_eq(&'#').is_some();
        spec.zero_pad = chars.next_if_eq(&'0').is_some();
        (spec.width, spec.width_expr) = self.parse_spec_size(frame, &mut chars, file, "width")?;
        spec.grouping = chars.next_if(|c| matches!(c, ',' | '_'));
        if chars.next_if_eq(&'.').is_some() {
            (spec.precision, spec.precision_expr) =
                self.parse_spec_size(frame, &mut chars, file, "precision")?;
        }
        spec.format_type = chars.next_if(|c| "bcdeEfFgGnosxX%".contains(*c));
        if chars.peek().is_some() {
            return Err(self.err(format!("Invalid format specifier: '{}'", format_str)));
        }
        Ok(spec)
    }

    fn parse_spec_size(
        &mut self,
        frame: &HoleFrame,
        chars: &mut std::iter::Peekable<std::str::Chars>,
        file: Ustr,
        label: &str,
    ) -> PResult<(Option<usize>, Option<Idx<ExprData>>)> {
        if chars.peek() != Some(&'{') {
            let digits = take_ascii_digits(chars);
            if digits.is_empty() {
                return Ok((None, None));
            }
            let size = digits
                .parse()
                .map_err(|_| self.err(format!("format {label} overflow")))?;
            return Ok((Some(size), None));
        }
        let format_str = &frame.text;
        let brace_idx = format_str.chars().count() - chars.clone().count();
        let brace_byte = format_str
            .char_indices()
            .nth(brace_idx)
            .map(|(b, _)| b)
            .unwrap_or(format_str.len());
        chars.next(); // `{`
        let src = take_balanced_brace_body(chars, format_str)?;
        let map = identity_map(frame.map[0] + brace_byte + 1, src.len());
        let value =
            self.parse_interp_subexpr(&src, file, &map, frame.line, frame.column + brace_idx + 1)?;
        Ok((None, Some(value)))
    }

    fn struct_literal_follows(&self) -> bool {
        self.allow_literals
            && self.kind() == TokenKind::OpenCurly
            && self.prev_trailing_newlines() == 0
    }

    #[allow(clippy::type_complexity)]
    fn parse_struct_fields(
        &mut self,
    ) -> PResult<(Vec<Idx<ExprData>>, Vec<(Vec<Ident>, Idx<ExprData>)>)> {
        self.expect(TokenKind::OpenCurly)?;
        self.delimiter_depth += 1;
        let mut bases = Vec::new();
        let mut fields = Vec::new();
        while self.kind() != TokenKind::CloseCurly && !self.at_end() {
            self.skip_comments();
            if self.kind() == TokenKind::CloseCurly {
                break;
            }
            if self.kind() == TokenKind::DoubleDot {
                self.advance();
                if !fields.is_empty() {
                    return Err(self.err("spread base `..` must come before field overrides"));
                }
                let base = self.with_allow_literals(true, |p| p.parse_expr(0))?;
                bases.push(base);
                self.skip_comments();
                if !self.eat(TokenKind::Comma) {
                    break;
                }
                continue;
            }
            let mut names = vec![self.expect_ident()?];
            while self.kind() == TokenKind::Plus {
                self.advance();
                names.push(self.expect_ident()?);
            }
            let value = if self.eat(TokenKind::Colon) {
                self.with_allow_literals(true, |p| p.parse_expr(0))?
            } else {
                let n = names[0].clone();
                self.ast.expr(n.span, ExprKind::Ident(n))
            };
            fields.push((names, value));
            self.skip_comments();
            if !self.eat(TokenKind::Comma) {
                if matches!(self.kind(), TokenKind::Ident(_) | TokenKind::DoubleDot) {
                    self.ast
                        .recovery_errors
                        .push(self.err("expected `,` between struct fields"));
                    continue;
                }
                break;
            }
        }
        self.skip_comments();
        self.delimiter_depth -= 1;
        self.expect(TokenKind::CloseCurly)?;
        Ok((bases, fields))
    }

    fn parse_match(&mut self) -> PResult<Idx<ExprData>> {
        let start = self.span();
        self.expect(TokenKind::Match)?;
        let scrutinee = self.with_allow_literals(false, |p| p.parse_expr(0))?;
        self.expect(TokenKind::OpenCurly)?;
        self.brace_depth += 1;
        let mut arms = Vec::new();
        while self.kind() != TokenKind::CloseCurly && !self.at_end() {
            self.skip_comments();
            if self.kind() == TokenKind::CloseCurly {
                break;
            }
            let pattern = self.parse_match_pattern()?;
            let guard = if self.kind() == TokenKind::If {
                self.advance();
                let saved = std::mem::replace(&mut self.arrow_lambda_blocked, true);
                let g = self.with_allow_literals(false, |p| p.parse_expr(0));
                self.arrow_lambda_blocked = saved;
                Some(g?)
            } else {
                None
            };
            self.skip_comments(); // `pat /* c */ =>`
            self.expect(TokenKind::FatArrow)?;
            let body = if self.kind() == TokenKind::OpenCurly {
                self.parse_block()?
            } else {
                let stmt_span = self.span();
                let stmt = match self.kind() {
                    TokenKind::Break | TokenKind::Continue | TokenKind::Return => {
                        self.parse_statement()?
                    }
                    _ => {
                        // A bare arm body allows struct literals (`=> S { .. }`,
                        // Rust parity): the `{` can't open a block here (the block
                        // form is the `OpenCurly` branch above), so it is a literal.
                        let e = self.with_allow_literals(true, |p| p.parse_expr(0))?;
                        let s = self.span_from(stmt_span);
                        self.ast.stmt(s, StatementKind::Expression(e))
                    }
                };
                let body_span = self.span_from(stmt_span);
                self.ast.block(body_span, None, vec![stmt])
            };
            arms.push(MatchArm {
                pattern,
                guard,
                body,
            });
            self.eat(TokenKind::Comma);
        }
        self.brace_depth -= 1;
        self.expect(TokenKind::CloseCurly)?;
        let span = self.span_from(start);
        Ok(self.ast.expr(span, ExprKind::Match { scrutinee, arms }))
    }

    fn parse_match_pattern(&mut self) -> PResult<Pattern> {
        let first = self.parse_pattern()?;
        if self.kind() == TokenKind::BitOr {
            let mut alts = vec![first];
            while self.eat(TokenKind::BitOr) {
                alts.push(self.parse_pattern()?);
            }
            Ok(Pattern::Or(alts))
        } else {
            Ok(first)
        }
    }

    /// The formatter cannot round-trip a comment inside a pattern, so the
    /// grammar does not take one: say that, rather than leaking the token.
    fn reject_pattern_comment(&mut self) -> PResult<()> {
        if matches!(self.kind(), TokenKind::Comment(_)) {
            return Err(self.err(
                "a comment cannot appear inside a pattern; put it above the statement".to_string(),
            ));
        }
        Ok(())
    }

    fn parse_pattern(&mut self) -> PResult<Pattern> {
        self.reject_pattern_comment()?;
        if matches!(self.kind(), TokenKind::DoubleDot) {
            self.advance();
            let inclusive = self.eat(TokenKind::Assign);
            let end = self.plain_literal();
            return Ok(Pattern::Range {
                start: None,
                end,
                inclusive,
            });
        }
        if let Some(lit) = self.plain_literal() {
            if matches!(self.kind(), TokenKind::DoubleDot) {
                self.advance();
                // `..=` (inclusive) lexes as `DoubleDot` + `Assign`. The end is
                // optional: `N..` (end-less) yields `end: None`.
                let inclusive = self.eat(TokenKind::Assign);
                let end = self.plain_literal();
                return Ok(Pattern::Range {
                    start: Some(lit),
                    end,
                    inclusive,
                });
            }
            return Ok(Pattern::Literal(lit));
        }
        match self.kind() {
            TokenKind::OpenParen => Ok(Pattern::Tuple(self.parse_pattern_tuple()?)),
            TokenKind::Ident(name) if name.as_str() == "_" => {
                self.advance();
                Ok(Pattern::Wildcard)
            }
            TokenKind::Ident(name) => {
                let ident_span = self.span();
                self.advance();
                let ident = Ident {
                    span: ident_span,
                    name,
                };
                let path = if self.kind() == TokenKind::DoubleColon {
                    self.parse_path_tail(ident)?
                } else {
                    Path {
                        from_root: false,
                        segments: vec![ident],
                    }
                };
                if self.kind() == TokenKind::OpenParen {
                    Ok(Pattern::PathVariant {
                        path,
                        args: Some(self.parse_pattern_tuple()?),
                    })
                } else if self.kind() == TokenKind::OpenCurly {
                    let (fields, rest) = self.parse_pattern_record_fields()?;
                    Ok(Pattern::PathRecord { path, fields, rest })
                } else if path.from_root || path.segments.len() > 1 {
                    Ok(Pattern::PathVariant { path, args: None })
                } else {
                    Ok(Pattern::Ident(path.segments.into_iter().next().unwrap()))
                }
            }
            other => Err(self.err(format!("unsupported pattern: {:?}", other))),
        }
    }

    fn parse_pattern_tuple(&mut self) -> PResult<Vec<Pattern>> {
        self.advance(); // `(`
        let patterns = self.parse_bare_comma_list(TokenKind::CloseParen, Self::parse_pattern)?;
        self.expect(TokenKind::CloseParen)?;
        Ok(patterns)
    }

    fn parse_pattern_record_fields(&mut self) -> PResult<(Vec<(Ident, Pattern)>, bool)> {
        self.expect(TokenKind::OpenCurly)?;
        let mut fields = Vec::new();
        let mut rest = false;
        while self.kind() != TokenKind::CloseCurly && !self.at_end() {
            self.reject_pattern_comment()?;
            if self.kind() == TokenKind::DoubleDot {
                self.advance();
                rest = true;
                break;
            }
            let field = self.expect_ident()?;
            let pat = if self.eat(TokenKind::Colon) {
                self.parse_pattern()?
            } else {
                Pattern::Ident(field.clone())
            };
            fields.push((field, pat));
            self.reject_pattern_comment()?;
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::CloseCurly)?;
        Ok((fields, rest))
    }

    fn plain_literal(&mut self) -> Option<Idx<ExprData>> {
        let span = self.span();
        let kind = match self.kind() {
            TokenKind::Int(n, t) => ExprKind::Int(n, t),
            TokenKind::Float(f) => ExprKind::Float(f),
            TokenKind::String(s) => ExprKind::String(s),
            TokenKind::True => ExprKind::Bool(true),
            TokenKind::False => ExprKind::Bool(false),
            TokenKind::Null => ExprKind::Null,
            _ => return None,
        };
        self.advance();
        Some(self.ast.expr(span, kind))
    }

    fn looks_like_lambda_params(&self) -> bool {
        let at = |i: usize| self.tokens.get(i).map(|t| t.kind).unwrap_or(TokenKind::EOF);
        let mut i = self.pos;
        let mut depth = 0i32;
        loop {
            match at(i) {
                TokenKind::OpenParen | TokenKind::OpenSquare | TokenKind::OpenCurly => depth += 1,
                TokenKind::CloseParen | TokenKind::CloseSquare | TokenKind::CloseCurly => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                TokenKind::EOF => return false,
                _ => {}
            }
            i += 1;
        }
        at(i) == TokenKind::FatArrow
    }

    fn finish_lambda(&mut self, params: Vec<LambdaParam>, start: Span) -> PResult<Idx<ExprData>> {
        self.expect(TokenKind::FatArrow)?;
        let body = if self.kind() == TokenKind::OpenCurly {
            self.parse_block()?
        } else {
            let e = self.with_allow_literals(true, |p| p.parse_expr(0))?;
            let body_span = self.ast.exprs[e].span;
            let stmt = self.ast.stmt(
                body_span,
                StatementKind::Return {
                    keyword_span: None,
                    value: Some(e),
                },
            );
            self.ast.block(body_span, None, vec![stmt])
        };
        let span = self.span_from(start);
        Ok(self.ast.expr(
            span,
            ExprKind::Lambda {
                params,
                return_ty: None,
                body,
            },
        ))
    }

    fn condition_has_let(&self) -> bool {
        let mut depth = 0i32;
        let mut i = self.pos;
        while let Some(tok) = self.tokens.get(i) {
            match tok.kind {
                TokenKind::OpenParen | TokenKind::OpenSquare => depth += 1,
                TokenKind::CloseParen | TokenKind::CloseSquare => depth -= 1,
                TokenKind::OpenCurly if depth == 0 => return false,
                TokenKind::Let if depth == 0 => return true,
                TokenKind::EOF => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    fn parse_if(&mut self) -> PResult<Idx<ExprData>> {
        let start = self.span();
        self.expect(TokenKind::If)?;

        // Binding form `if [cond &&]* let pat = value [&& ..] { .. }` — an
        // if-let-chain whenever the condition contains a top-level `let`, even
        // when it opens with a plain condition (`if flag && let x = ..`).
        self.skip_comments_in_expr();
        if self.kind() == TokenKind::Let || self.condition_has_let() {
            return self.parse_if_let(start);
        }

        let mut branches = Vec::new();
        let cond = self.with_allow_literals(false, |p| p.parse_expr(0))?;
        let body = self.parse_block()?;
        branches.push((cond, body));

        let mut else_branch = None;
        while self.kind() == TokenKind::Else {
            self.advance();
            if self.eat(TokenKind::If) {
                let cond = self.with_allow_literals(false, |p| p.parse_expr(0))?;
                let body = self.parse_block()?;
                branches.push((cond, body));
            } else {
                else_branch = Some(self.parse_block()?);
                break;
            }
        }
        let span = self.span_from(start);
        Ok(self.ast.expr(
            span,
            ExprKind::If {
                branches,
                else_branch,
            },
        ))
    }

    fn parse_if_let(&mut self, start: Span) -> PResult<Idx<ExprData>> {
        let mut clauses: Vec<IfLetClause> = Vec::new();
        loop {
            self.skip_comments_in_expr();
            if self.kind() == TokenKind::Let {
                self.advance();
                let pattern = self.parse_pattern()?;
                self.expect(TokenKind::Assign)?;
                let value = self.with_allow_literals(false, |p| p.parse_expr(16))?;
                clauses.push(IfLetClause::Binding { pattern, value });
            } else {
                let cond = self.with_allow_literals(false, |p| p.parse_expr(16))?;
                clauses.push(IfLetClause::Condition(cond));
            }
            if self.kind() == TokenKind::And {
                self.advance();
            } else {
                break;
            }
        }

        let then_block = self.parse_block()?;
        self.skip_comments_in_expr();
        let else_block = if self.kind() == TokenKind::Else {
            self.advance();
            Some(self.parse_block()?)
        } else {
            None
        };

        let span = self.span_from(start);
        Ok(self.ast.expr(
            span,
            ExprKind::IfLetChain {
                clauses,
                then_block,
                else_block,
            },
        ))
    }

    fn node_span(&self, left: Idx<ExprData>, end: Span) -> Span {
        self.merge_spans(self.ast.exprs[left].span, end)
    }

    fn skip_comments_in_expr(&mut self) {
        // Only skip a comment if an operator/continuation follows, matching
        // rock's `skip_comments_if_followed_by_match`. Conservative: skip
        // when the token after the comment continues the expression.
        while matches!(self.kind(), TokenKind::Comment(_)) {
            let next = self.peek_kind();
            // A fresh-line expression starter after a comment belongs to the next statement.
            let next_on_fresh_line = self
                .tokens
                .get(self.pos)
                .map(|t| t.trailing_newlines)
                .unwrap_or(0)
                > 0
                && (self.delimiter_depth == 0 || self.brace_depth > 0);
            if op_starts_expr(next) && next_on_fresh_line {
                break;
            }
            if next.is_op() {
                self.advance();
            } else {
                break;
            }
        }
    }
}

fn take_ascii_digits(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut digits = String::new();
    while let Some(c) = chars.next_if(char::is_ascii_digit) {
        digits.push(c);
    }
    digits
}

fn primitive_type(name: &str) -> Option<TypeExprKind> {
    Some(match name {
        "any" => TypeExprKind::Any,
        "string" => TypeExprKind::String,
        "int" => TypeExprKind::Int,
        "uint" => TypeExprKind::Uint,
        "float" => TypeExprKind::Float,
        "f64" => TypeExprKind::F64,
        "f32" => TypeExprKind::F32,
        "bool" => TypeExprKind::Bool,
        _ => return None,
    })
}

fn take_balanced_brace_body(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    full_spec: &str,
) -> PResult<String> {
    let mut body = String::new();
    let mut depth: usize = 1;
    for c in chars.by_ref() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(body);
                }
            }
            _ => {}
        }
        body.push(c);
    }
    Err(CompilerError::new_with_label(
        Span::unknown(),
        format!(
            "unbalanced `{{` in format specifier: '{}' (expected matching `}}`)",
            full_spec
        ),
    ))
}

/// A tuple index after `.`/`?.` is a plain decimal digit run. The tokenizer
/// lexes a sign into the literal, so `t.-1` would otherwise wrap to a huge
/// index.
fn is_tuple_index(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

fn op_starts_expr(op: TokenKind) -> bool {
    matches!(
        op,
        TokenKind::PlusPlus
            | TokenKind::MinusMinus
            | TokenKind::OpenParen
            | TokenKind::OpenSquare
            // `@` on a fresh line starts an annotation, never an `expr @ lambda` glued to
            // the previous line. All real `@`-operator uses are same-line.
            | TokenKind::At
    )
}

fn no_parens_arg_start(op: TokenKind) -> bool {
    matches!(
        op,
        TokenKind::Ident(_)
            | TokenKind::Int(_, _)
            | TokenKind::Float(_)
            | TokenKind::String(_)
            | TokenKind::RawString(_)
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Null
            | TokenKind::Bang
    )
}

fn no_parens_arg_continue(op: TokenKind) -> bool {
    no_parens_arg_start(op)
        || matches!(
            op,
            TokenKind::OpenParen
                | TokenKind::OpenSquare
                | TokenKind::OpenCurly
                | TokenKind::PlusPlus
                | TokenKind::MinusMinus
        )
}
