//! Recursive-descent + Pratt parser producing the arena AST.
//!
//! Precedence comes from `rock-tokenizer`'s binding-power tables
//! (`TokenKind::{prefix,infix,postfix}_bp`) — the same source rock's
//! parser uses, so the A/B isolates allocation strategy, not grammar.
//! This is the walking-skeleton subset (plan milestone 1): functions,
//! `let`/`return`/expression statements, and the common expression
//! forms. Unsupported constructs return an error so the A/B harness
//! reports coverage honestly rather than silently parsing less.

use crate::ast::*;
use rock_core::{CompilerError, Span};
use rock_tokenizer::{Token, TokenKind, Tokenizer};
use ustr::Ustr;

pub struct Parser {
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
    /// While parsing a match-arm guard, suppress the bare-ident (`name => ..`)
    /// and paren-form (`(name) => ..`) lambda commits so the arm's `=>` isn't
    /// swallowed. Mirrors rock's `arrow_lambda_blocked`. The flag persists
    /// through binary-op RHS / postfix recursion (covers `name && x`,
    /// `name.field`); inside parens the arm `=>` is unreachable so it's not
    /// reset at delimiter boundaries. See `docs/match-guard-parse-ambiguity.md`.
    arrow_lambda_blocked: bool,
    /// Set by `parse_operator` so `parse_item` can replicate rock's implicit
    /// `@inline` on block-bodied operator overloads (the `@_transparent` model).
    last_was_operator: bool,
    /// Per-module counter for `?` holes, bumped on every type AND expr hole in
    /// parse order — the mirror of rock's `AstBuilder::gen_hole_id`. Only
    /// expr-hole ids surface in the s-expr (`?{id}`), but type holes must still
    /// advance it so the numbering stays in lockstep with rock.
    hole_counter: u32,
}

type PResult<T> = Result<T, CompilerError>;

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        let eof_span = tokens.last().map(|t| t.span).unwrap_or_else(Span::unknown);
        let ast = Ast::with_capacity(tokens.len());
        Self {
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
        }
    }

    /// Next `?`-hole id (type or expr), mirroring rock's `gen_hole_id`.
    fn gen_hole_id(&mut self) -> u32 {
        let id = self.hole_counter;
        self.hole_counter += 1;
        id
    }

    /// Run `f` with struct-literal parsing forced on/off (condition heads
    /// turn it off; delimited contexts turn it back on), restoring after.
    fn with_allow_literals<T>(&mut self, allow: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.allow_literals, allow);
        let out = f(self);
        self.allow_literals = saved;
        out
    }

    /// Parse the whole token stream as a module, consuming the parser and
    /// returning the populated arena AST.
    ///
    /// Item-level recovery: top-level constructs the skeleton subset does
    /// not yet handle (struct/enum/global/annotations/...) are skipped to
    /// the next `fn`, so the `fn`s skull *does* understand still get parsed.
    /// Within a `fn`, `parse_block` recovers at statement level (see
    /// `recover_to_next_statement`): a broken statement drops only itself, so
    /// the surrounding body survives and scope-based LSP features keep working
    /// on broken code. An item whose *header* (signature) fails to parse is
    /// still dropped whole. Valid input never enters either recovery path, so
    /// the A/B parse oracle stays byte-faithful.
    pub fn parse_module(mut self) -> PResult<Ast> {
        loop {
            self.skip_comments_as_items();
            if self.at_end() {
                break;
            }
            match self.parse_item() {
                Ok(item) => self.ast.items.push(item),
                Err(e) => {
                    // Record EVERY failed item, not just `fn`: a malformed
                    // struct/enum/newtype/global/decorator/macro/directive must
                    // be surfaced on `recovery_errors` (the downstream compile gate
                    // rejects on it), not silently dropped. Valid input never reaches
                    // here (every
                    // supported item start has a branch in `parse_item_inner`), so
                    // this stays empty for well-formed programs.
                    self.ast.recovery_errors.push(e);
                    self.recover_to_next_item();
                }
            }
        }
        Ok(self.ast)
    }

    /// Parse exactly ONE top-level item from the token stream and return the
    /// populated single-item arena AST. The incremental driver tokenizes just an
    /// edited item's text (with `Tokenizer::new_with_offset`, so spans are
    /// absolute) and re-parses it here, feeding the result straight to the
    /// per-body lowerer -- no whole-module reparse, no shared-arena splice.
    /// Errors (a malformed body) propagate so the caller falls back to a full
    /// reparse rather than installing a half-item. `docs/skull-incremental-parse-plan.md`.
    pub fn parse_single_item(mut self) -> PResult<Ast> {
        self.skip_comments_as_items();
        let item = self.parse_item()?;
        self.ast.items.push(item);
        Ok(self.ast)
    }

    /// Skip a balanced span of tokens up to the next top-level `fn`. Used to
    /// step over unsupported items (and malformed `fn`s) without aborting
    /// the whole module.
    fn recover_to_next_item(&mut self) {
        let mut depth: i32 = 0;
        // Always consume at least one token so recovery makes progress.
        if !self.at_end() {
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

    /// Tokens that can begin a statement -- the resync targets for
    /// statement-level error recovery.
    fn can_start_statement(&self, kind: TokenKind) -> bool {
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

    /// After a failed statement inside a block, skip tokens to the next
    /// statement boundary so the rest of the block still parses (a broken
    /// statement no longer drops the whole `fn` to item-level recovery).
    /// Returns `false` if the block's `}` or EOF is reached (the block loop
    /// should stop), `true` if a resync point was found. Like rock's
    /// `recover_to_next_statement`, brace depth is not tracked -- a stray inner
    /// `}` ends recovery, matching rock's behavior on the same input.
    fn recover_to_next_statement(&mut self) -> bool {
        while !self.at_end() {
            match self.kind() {
                TokenKind::Semicolon => {
                    self.advance();
                    return true;
                }
                TokenKind::CloseCurly | TokenKind::EOF => return false,
                k if self.can_start_statement(k) => return true,
                _ => self.advance(),
            }
        }
        false
    }

    // ---- token navigation -------------------------------------------------

    fn kind(&self) -> TokenKind {
        self.tokens.get(self.pos).map(|t| t.kind).unwrap_or(TokenKind::EOF)
    }

    fn span(&self) -> Span {
        self.tokens.get(self.pos).map(|t| t.span).unwrap_or(self.eof_span)
    }

    /// Span of the most-recently-consumed token (`pos - 1`). A node's full
    /// extent runs from its first token (`start`, captured before consuming it)
    /// through this — `self.span()` is the *next*, unconsumed token, so it
    /// overshoots.
    fn prev_span(&self) -> Span {
        if self.pos == 0 {
            self.span()
        } else {
            self.tokens.get(self.pos - 1).map(|t| t.span).unwrap_or(self.eof_span)
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

    /// Full-extent span from a node's first token (`start`) through the last
    /// token consumed so far. The canonical way to span a node: capture
    /// `let start = self.span();` before consuming, build, then `span_from(start)`.
    fn span_from(&self, start: Span) -> Span {
        self.merge_spans(start, self.prev_span())
    }

    fn peek_kind(&self) -> TokenKind {
        self.tokens.get(self.pos + 1).map(|t| t.kind).unwrap_or(TokenKind::EOF)
    }

    /// Is the token after the current one an identifier whose first char is
    /// ASCII-uppercase? Distinguishes `Color::Red` (qualified) from
    /// `value :: int` (ascription).
    fn peek_is_uppercase_ident(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Ident(n)
            if n.as_str().chars().next().is_some_and(|c| c.is_ascii_uppercase()))
    }

    /// Newlines that followed the *previous* token (the tokenizer folds
    /// `Newline` into `Token::trailing_newlines`).
    fn prev_trailing_newlines(&self) -> usize {
        if self.pos == 0 {
            0
        } else {
            self.tokens.get(self.pos - 1).map(|t| t.trailing_newlines).unwrap_or(0)
        }
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
            TokenKind::GreaterThan => self.expect(TokenKind::GreaterThan),
            TokenKind::RightShift => {
                let span = self.span();
                self.advance();
                self.tokens.insert(
                    self.pos,
                    Token { kind: TokenKind::GreaterThan, span, trailing_newlines: 0 },
                );
                Ok(span)
            }
            other => Err(self.err(format!("expected GreaterThan, got {:?}", other))),
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
            self.ast.items.push(Item { annotations: Vec::new(),
                span,
                kind: ItemKind::Comment(text),
            });
            self.advance();
        }
        // Drop stray top-level semicolons.
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

    // ---- items ------------------------------------------------------------

    fn parse_item(&mut self) -> PResult<Item> {
        // Leading `@name[(args)]` annotations are captured and attached to the
        // parsed item (rendered in the s-expr, matching rock). rock's item span
        // covers the annotations too, so anchor the start at the first `@`.
        let item_start = self.span();
        let mut annotations = self.parse_item_annotations()?;
        // rock consumes `@guarded` / `@global_write` into flags (Global.is_guarded
        // etc.) and drops them from the annotation list — mirror that so they
        // don't render.
        let is_guarded = annotations.iter().any(|a| a.name.name.as_str() == "guarded");
        annotations.retain(|a| !matches!(a.name.name.as_str(), "guarded" | "global_write"));
        self.last_was_operator = false;
        let mut item = self.parse_item_inner()?;
        if is_guarded {
            if let ItemKind::Global(g) = &mut item.kind {
                g.is_guarded = true;
            }
        }
        // rock injects `@inline` on block-bodied operator overloads (no source
        // form) unless one is already present — replicate it so the A/B matches.
        if self.last_was_operator
            && matches!(&item.kind, ItemKind::Function(f) if f.body.rock_block().is_some())
            && !annotations.iter().any(|a| a.name.name.as_str() == "inline")
        {
            annotations.push(Annotation {
                span: item.span,
                name: Ident { span: item.span, name: Ustr::from("inline") },
                args: Vec::new(),
            });
        }
        item.annotations = annotations;
        // Widen the start to cover leading annotations (no-op when unannotated).
        item.span = self.merge_spans(item_start, item.span);
        Ok(item)
    }

    fn parse_item_inner(&mut self) -> PResult<Item> {
        match self.kind() {
            TokenKind::Fn => self.parse_fn(false, None),
            // `co fn name(...)` — coroutine; body may use `yield`. `co` is a
            // contextual keyword: only reserved immediately before `fn`.
            TokenKind::Ident(n) if n.as_str() == "co" && matches!(self.peek_kind(), TokenKind::Fn) => {
                self.advance();
                self.parse_fn(true, None)
            }
            TokenKind::Operator => self.parse_operator(false, None),
            // `macro name(params) [-> ret] { body }` — rock folds these to
            // `Function` items (block body = expansion template).
            TokenKind::Macro => {
                let start = self.span();
                self.advance();
                let name = self.expect_ident()?;
                self.finish_fn_def(name, start, true, false, None)
            }
            // `extern [(ffi, "lib") | (lua)] fn ..` — body-less declaration.
            TokenKind::Extern => {
                self.advance();
                let extern_body = self.parse_extern_specifier();
                self.parse_fn(false, Some(extern_body))
            }
            // `inline(<backend>) fn|operator ..` — Lua body (`= """..."""`).
            TokenKind::Ident(n) if n.as_str() == "inline" => {
                self.advance();
                if self.kind() == TokenKind::OpenParen {
                    self.skip_balanced_parens();
                }
                // The Lua source is captured by `finish_fn_def` from the
                // `= """..."""` body, so no body marker is needed here.
                match self.kind() {
                    TokenKind::Operator => self.parse_operator(false, None),
                    TokenKind::Ident(n) if n.as_str() == "co" => {
                        self.advance();
                        self.parse_fn(true, None)
                    }
                    _ => self.parse_fn(false, None),
                }
            }
            TokenKind::Struct => self.parse_struct(false),
            // `value Name { .. }` — contextual keyword (only before a type name).
            TokenKind::Ident(n)
                if n.as_str() == "value" && matches!(self.peek_kind(), TokenKind::Ident(_)) =>
            {
                self.parse_struct(true)
            }
            TokenKind::Enum => self.parse_enum(),
            TokenKind::Newtype => self.parse_newtype(),
            TokenKind::Deco => self.parse_decorator(),
            TokenKind::Global => self.parse_global(false),
            TokenKind::Const => self.parse_global(true),
            TokenKind::DirectiveIf
            | TokenKind::DirectiveElseIf
            | TokenKind::DirectiveElse
            | TokenKind::DirectiveEnd => self.parse_compiler_directive(),
            // Top-level macro invocation `name(args)` / `name!(args)`.
            TokenKind::Ident(_)
                if matches!(self.peek_kind(), TokenKind::OpenParen | TokenKind::Bang) =>
            {
                self.parse_top_level_invocation()
            }
            other => Err(self.err(format!("unsupported item start: {:?}", other))),
        }
    }

    /// `name(args)` / `name!(args)` at module scope — a macro invocation. The
    /// sigil (`!`) enables extended-parsing so type-shaped args (`Foo<T>`,
    /// `[T]`, `(T, U)`) survive as exprs for `expr_to_type_expr` to recover.
    fn parse_top_level_invocation(&mut self) -> PResult<Item> {
        let start = self.span();
        let name = self.expect_ident()?;
        let sigil = self.eat(TokenKind::Bang);
        self.expect(TokenKind::OpenParen)?;
        let mut args = Vec::new();
        while self.kind() != TokenKind::CloseParen && !self.at_end() {
            self.skip_comments();
            if self.kind() == TokenKind::CloseParen {
                break;
            }
            args.push(self.parse_macro_arg(sigil)?);
            self.skip_comments();
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::CloseParen)?;
        let span = self.span_from(start);
        Ok(Item {
            annotations: Vec::new(),
            span,
            kind: ItemKind::MacroInvocation { name, args, sigil },
        })
    }

    /// `name!(args)` in expression position — the args are extended-parsed
    /// (type-shaped forms survive as exprs), but the result is a plain call.
    fn parse_sigil_call(&mut self, func: Idx<ExprData>) -> PResult<Idx<ExprData>> {
        let start = self.ast.exprs[func].span;
        self.expect(TokenKind::OpenParen)?;
        self.delimiter_depth += 1;
        let mut args = Vec::new();
        while self.kind() != TokenKind::CloseParen && !self.at_end() {
            self.skip_comments();
            if self.kind() == TokenKind::CloseParen {
                break;
            }
            let is_splat = self.eat(TokenKind::TripleDot); // `inner!(...rest)`
            let value = self.parse_macro_arg(true)?;
            args.push(FunctionCallArg { name: None, value, is_splat });
            self.skip_comments();
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.delimiter_depth -= 1;
        self.expect(TokenKind::CloseParen)?;
        let span = self.span_from(start);
        Ok(self.ast.expr(span, ExprKind::FunctionCall { func, args, syntax: CallSyntax::Normal }))
    }

    /// One macro-invocation argument. Non-sigil args are ordinary expressions;
    /// sigil args are type-shaped — `Name<A,B>`, `[T]`, `(T,U)`, and a trailing
    /// `?` (nullable) are parsed recursively so `expr_to_type_expr` can recover
    /// arbitrarily nested types (`[BoxL<int>]`, `Map<int, BoxL<int?>>`).
    fn parse_macro_arg(&mut self, sigil: bool) -> PResult<Idx<ExprData>> {
        if !sigil {
            return self.with_allow_literals(true, |p| p.parse_expr(0));
        }
        // Cursor-backtracking (rock parity): try the type-shaped parse; accept
        // it ONLY if it lands cleanly at an arg boundary. Otherwise reset and
        // parse an ordinary expression — so `(a < b)` (a comparison) isn't
        // mis-read as the generic `a<b>`.
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

    /// Parse a type-shaped sigil arg (`Name<..>`, `[T]`, `(T,U)`, `T?`). Errors
    /// (or lands off an arg boundary) when the form isn't actually a type — the
    /// caller backtracks to ordinary expression parsing.
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

    /// A type-shaped sigil arg atom: `Name<..>` (as a `FunctionCall`), `[T]`,
    /// `(T, U)` / `(T)`, or a plain ident/builtin (parsed above comparison bp so
    /// a closing `>` ends it).
    fn parse_sigil_atom(&mut self) -> PResult<Idx<ExprData>> {
        let start = self.span();
        match self.kind() {
            TokenKind::Ident(name) if self.peek_kind() == TokenKind::LessThan => {
                self.advance(); // name
                self.advance(); // `<`
                let func = self.ast.expr(start, ExprKind::Ident(Ident { span: start, name }));
                let mut targs = Vec::new();
                while !matches!(self.kind(), TokenKind::GreaterThan | TokenKind::RightShift)
                    && !self.at_end()
                {
                    let v = self.parse_macro_arg(true)?;
                    targs.push(FunctionCallArg { name: None, value: v, is_splat: false });
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
                self.expect_generic_close()?;
                let span = self.span_from(start);
                Ok(self.ast.expr(
                    span,
                    ExprKind::FunctionCall { func, args: targs, syntax: CallSyntax::Normal },
                ))
            }
            TokenKind::OpenSquare => {
                self.advance();
                let mut elems = Vec::new();
                while self.kind() != TokenKind::CloseSquare && !self.at_end() {
                    elems.push(self.parse_macro_arg(true)?);
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(TokenKind::CloseSquare)?;
                let span = self.span_from(start);
                Ok(self.ast.expr(span, ExprKind::ArrayLiteral(elems)))
            }
            TokenKind::OpenParen => {
                self.advance();
                let mut elems = Vec::new();
                while self.kind() != TokenKind::CloseParen && !self.at_end() {
                    elems.push(self.parse_macro_arg(true)?);
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(TokenKind::CloseParen)?;
                let span = self.span_from(start);
                let kind = if elems.len() == 1 {
                    ExprKind::Paren(elems[0])
                } else {
                    ExprKind::Tuple(elems)
                };
                Ok(self.ast.expr(span, kind))
            }
            // Plain ident / builtin: above comparison bp so `>` / `,` end it.
            _ => self.with_allow_literals(true, |p| p.parse_expr(26)),
        }
    }

    /// After `extern`, read the optional `(ffi, "lib")` / `(lua)` specifier and
    /// return the body kind for the (body-less) declaration.
    fn parse_extern_specifier(&mut self) -> FunctionBody {
        if self.kind() != TokenKind::OpenParen {
            return FunctionBody::ExternC;
        }
        self.advance(); // `(`
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
            TokenKind::Ident(n) if n.as_str() == "lua" => FunctionBody::ExternLua,
            _ => FunctionBody::ExternC,
        };
        // Consume to the closing `)`.
        while !matches!(self.kind(), TokenKind::CloseParen | TokenKind::EOF) {
            self.advance();
        }
        self.eat(TokenKind::CloseParen);
        kind
    }

    /// Parse leading item annotations: `@name` / `@name(arg, name=arg, ...)`,
    /// repeated, comments allowed between. Arg values parse as expressions.
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
                while self.kind() != TokenKind::CloseParen && !self.at_end() {
                    self.skip_comments();
                    // Named arg `name = value`.
                    let arg_name = if matches!(self.kind(), TokenKind::Ident(_))
                        && self.peek_kind() == TokenKind::Assign
                    {
                        let n = self.expect_ident()?;
                        self.expect(TokenKind::Assign)?;
                        Some(n)
                    } else {
                        None
                    };
                    let value = self.with_allow_literals(true, |p| p.parse_expr(0))?;
                    args.push(AnnoArg { name: arg_name, value });
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
                self.delimiter_depth -= 1;
                self.expect(TokenKind::CloseParen)?;
            }
            let span = self.span_from(start);
            anns.push(Annotation { span, name, args });
            self.skip_comments();
        }
        Ok(anns)
    }

    /// Skip a balanced `( .. )` starting at the current `(`.
    fn skip_balanced_parens(&mut self) {
        let mut depth = 0i32;
        loop {
            match self.kind() {
                TokenKind::OpenParen => depth += 1,
                TokenKind::CloseParen => {
                    depth -= 1;
                    self.advance();
                    if depth <= 0 {
                        return;
                    }
                    continue;
                }
                TokenKind::EOF => return,
                _ => {}
            }
            self.advance();
        }
    }

    fn parse_fn(&mut self, is_coroutine: bool, extern_body: Option<FunctionBody>) -> PResult<Item> {
        let start = self.span();
        self.expect(TokenKind::Fn)?;
        let name = self.expect_ident()?;
        self.finish_fn_def(name, start, false, is_coroutine, extern_body)
    }

    /// `operator [(<backend>)] <sym>(params) -> ret { body }` folds to a
    /// function named `__op_<sym>` (rock's `OperatorSymbol::function_name`),
    /// so the body s-expr lines up with rock's.
    fn parse_operator(&mut self, is_coroutine: bool, extern_body: Option<FunctionBody>) -> PResult<Item> {
        self.last_was_operator = true;
        let start = self.span();
        self.expect(TokenKind::Operator)?;
        // Optional backend specifier `operator(lua)`. A non-block body (`= "..."`)
        // is captured as Lua by `finish_fn_def` regardless, so the specifier only
        // needs consuming, not interpreting.
        if self.kind() == TokenKind::OpenParen {
            self.advance();
            while self.kind() != TokenKind::CloseParen && !self.at_end() {
                self.advance();
            }
            self.expect(TokenKind::CloseParen)?;
        }
        self.skip_comments();
        let sym_span = self.span();
        let op_name = match self.kind() {
            TokenKind::Plus => "__op_add",
            TokenKind::Minus => "__op_sub",
            TokenKind::Mul => "__op_mul",
            TokenKind::Div => "__op_div",
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
        let name = Ident { span: sym_span, name: Ustr::from(op_name) };
        self.finish_fn_def(name, start, false, is_coroutine, extern_body)
    }

    /// Generic params + `(params)` + optional `-> ret` + body, shared by
    /// `parse_fn` and `parse_operator`. The body is `Rock` for a `{ }` block,
    /// `Lua` for an `= "..."` inline body, else `extern_body` (a body-less
    /// extern decl) — `None` for the non-extern callers, who never reach it.
    fn finish_fn_def(
        &mut self,
        name: Ident,
        start: Span,
        is_macro: bool,
        is_coroutine: bool,
        extern_body: Option<FunctionBody>,
    ) -> PResult<Item> {
        let generics = self.parse_generics();
        let params = self.parse_params()?;
        self.skip_comments(); // `f() /* c */ -> T`
        let return_ty = if self.eat(TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };

        // Body: a `{ }` block (Rock), or a non-block body — `= <inline lua>` /
        // extern decl. The non-Rock blob is opaque (parsed-past, not modelled).
        // A macro body may interleave item declarations (`fn`/`struct`/..) with
        // the trailing expression; those go to `macro_items`.
        let (body, macro_items) = if self.kind() == TokenKind::OpenCurly {
            if is_macro {
                let (block, items) = self.parse_macro_body()?;
                (FunctionBody::Rock(block), items)
            } else {
                (FunctionBody::Rock(self.parse_block()?), Vec::new())
            }
        } else {
            // Non-block body: an inline backend body (`= """<source>"""`, captured
            // verbatim like rock's `BodySource::Lua`) or a body-less extern decl
            // (the `extern_body` the caller determined).
            let body = if self.eat(TokenKind::Assign) {
                self.skip_comments();
                let src = match self.kind() {
                    TokenKind::String(s) | TokenKind::RawString(s) => {
                        self.advance();
                        s
                    }
                    // Malformed inline body (not a string literal): parse-past for
                    // recovery and keep an empty source.
                    _ => {
                        self.with_allow_literals(true, |p| p.parse_expr(0))?;
                        Ustr::from("")
                    }
                };
                FunctionBody::Lua(src)
            } else {
                extern_body.unwrap_or(FunctionBody::ExternC)
            };
            self.eat(TokenKind::Semicolon);
            (body, Vec::new())
        };
        let span = self.span_from(start);
        // Record this macro's parameter reference spans (decl + template uses)
        // while we still hold the definition with its source spans -- expansion
        // would only see retagged clones. The IDE reads `Ast.macro_param_refs`.
        if is_macro {
            let refs = crate::macro_expand::index_macro_param_refs(
                &self.ast,
                &params,
                body.rock_block(),
                &macro_items,
            );
            self.ast.macro_param_refs.extend(refs);
        }
        Ok(Item { annotations: Vec::new(),
            span,
            kind: ItemKind::Function(FunctionDef {
                span,
                keyword_span: Some(start),
                name,
                generics,
                params,
                return_ty,
                body,
                is_macro,
                is_coroutine,
                is_synthetic: false,
                macro_items,
            }),
        })
    }

    /// Parse a macro body `{ ... }`: item declarations (`fn`/`inline(lua) fn`/
    /// `extern fn`/`struct`/`enum`/`newtype`, optionally annotated) collect into
    /// `Vec<Item>`; the remaining statements form the block whose trailing
    /// expression is the expression-position expansion. Mirrors rock's
    /// `parse_macro_body_block`.
    fn parse_macro_body(&mut self) -> PResult<(Idx<BlockData>, Vec<Item>)> {
        let start = self.expect(TokenKind::OpenCurly)?;
        self.brace_depth += 1;
        let mut statements = Vec::new();
        let mut items = Vec::new();
        loop {
            while self.eat(TokenKind::Semicolon) {}
            while let TokenKind::Comment(text) = self.kind() {
                let span = self.span();
                statements.push(self.ast.stmt(span, StatementKind::Comment(text)));
                self.advance();
            }
            if self.kind() == TokenKind::CloseCurly || self.at_end() {
                break;
            }
            if self.token_starts_macro_body_item() {
                items.push(self.parse_item()?);
                continue;
            }
            statements.push(self.parse_statement()?);
            self.consume_trailing_comments();
            while self.eat(TokenKind::Semicolon) {}
        }
        self.brace_depth -= 1;
        let close = self.expect(TokenKind::CloseCurly)?;
        let span = self.span_from(start);
        Ok((self.ast.block(span, Some((start, close)), statements), items))
    }

    /// Does the current token start an item declaration valid inside a macro
    /// body? (`@anno` / `fn` / `co` / `extern` / `struct` / `enum` / `newtype` /
    /// `inline(lua) fn`.) Mirrors rock's `token_starts_macro_body_item`.
    fn token_starts_macro_body_item(&self) -> bool {
        match self.kind() {
            TokenKind::At | TokenKind::Fn | TokenKind::Extern
            | TokenKind::Struct | TokenKind::Enum | TokenKind::Newtype => true,
            TokenKind::Ident(name) if name.as_str() == "co" => {
                matches!(self.peek_kind(), TokenKind::Fn)
            }
            TokenKind::Ident(name) if name.as_str() == "inline" => {
                self.peek_kind() == TokenKind::OpenParen
            }
            _ => false,
        }
    }

    /// Parse a `<T, U = Default, ...>` generic parameter list (empty if no `<`).
    /// Closes on `>` or a split `>>` (`expect_generic_close`).
    fn parse_generics(&mut self) -> Vec<Generic> {
        let mut out = Vec::new();
        if self.kind() != TokenKind::LessThan {
            return out;
        }
        self.advance(); // `<`
        while !matches!(self.kind(), TokenKind::GreaterThan | TokenKind::RightShift)
            && !self.at_end()
        {
            self.skip_comments();
            let name = match self.expect_ident() {
                Ok(id) => id,
                Err(_) => break,
            };
            let default = if self.eat(TokenKind::Assign) {
                self.parse_type().ok()
            } else {
                None
            };
            out.push(Generic { name, default });
            self.skip_comments(); // `<T /* c */>`
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        let _ = self.expect_generic_close();
        out
    }

    /// Parse a `(name: T [= default] [...], ...)` parameter list.
    fn parse_params(&mut self) -> PResult<Vec<Param>> {
        self.expect(TokenKind::OpenParen)?;
        let mut params = Vec::new();
        while self.kind() != TokenKind::CloseParen && !self.at_end() {
            self.skip_comments();
            if self.kind() == TokenKind::CloseParen {
                break;
            }
            let name = self.expect_ident()?;
            self.expect(TokenKind::Colon)?;
            // Variadic spelled either leading (`...T`, the stdlib form e.g.
            // `print(args: ...any)`) or trailing (`T...`).
            let leading_variadic = self.eat(TokenKind::TripleDot);
            let ty = self.parse_type()?;
            let is_variadic = leading_variadic || self.eat(TokenKind::TripleDot);
            let default = if self.eat(TokenKind::Assign) {
                Some(self.with_allow_literals(true, |p| p.parse_expr(0))?)
            } else {
                None
            };
            let span = self.span_from(name.span);
            params.push(Param { span, name, ty, default, is_variadic });
            self.skip_comments();
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.skip_comments();
        self.expect(TokenKind::CloseParen)?;
        Ok(params)
    }

    /// `struct Name<gen> { field: T [= default], ... }` (also `value struct`,
    /// entered with `value` already consumed and `is_value = true`).
    fn parse_struct(&mut self, is_value: bool) -> PResult<Item> {
        let start = self.span();
        // `value Name { .. }` (contextual keyword, no `struct`) vs `struct Name`.
        if is_value {
            self.advance(); // `value`
        } else {
            self.expect(TokenKind::Struct)?;
        }
        let name = self.expect_ident()?;
        let generics = self.parse_generics();
        let fields = self.parse_field_block()?;
        let span = self.span_from(start);
        Ok(Item { annotations: Vec::new(), span, kind: ItemKind::Struct(StructDef { span, keyword_span: Some(start), name, generics, fields, is_value }) })
    }

    /// `{ field: T [= default], ... }` — struct body / enum record variant.
    fn parse_field_block(&mut self) -> PResult<Vec<Field>> {
        self.expect(TokenKind::OpenCurly)?;
        self.brace_depth += 1;
        let mut fields = Vec::new();
        loop {
            self.skip_comments();
            if self.kind() == TokenKind::CloseCurly || self.at_end() {
                break;
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
            fields.push(Field { span, name, ty, default });
            self.skip_comments();
            // Item separator: comma or newline (both allowed).
            self.eat(TokenKind::Comma);
        }
        self.brace_depth -= 1;
        self.expect(TokenKind::CloseCurly)?;
        Ok(fields)
    }

    /// `enum Name<gen> { Variant, Variant(T, ..), Variant { f: T, .. }, ... }`.
    fn parse_enum(&mut self) -> PResult<Item> {
        let start = self.span();
        self.expect(TokenKind::Enum)?;
        let name = self.expect_ident()?;
        let generics = self.parse_generics();
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
                // Tuple variant — fields may be `name: T` (named positional) or
                // bare `T`; only the type matters for the s-expr.
                self.advance();
                self.delimiter_depth += 1;
                let mut tys = Vec::new();
                while self.kind() != TokenKind::CloseParen && !self.at_end() {
                    self.skip_comments();
                    if matches!(self.kind(), TokenKind::Ident(_))
                        && self.peek_kind() == TokenKind::Colon
                    {
                        self.advance(); // field name
                        self.advance(); // `:`
                    }
                    tys.push(self.parse_type()?);
                    self.skip_comments(); // `Rect(float /* c */, float /* c */)`
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
                self.delimiter_depth -= 1;
                self.expect(TokenKind::CloseParen)?;
                EnumVariantKind::Tuple(tys)
            } else if self.kind() == TokenKind::OpenCurly {
                EnumVariantKind::Record(self.parse_field_block()?)
            } else {
                EnumVariantKind::Unit
            };
            let span = self.span_from(vname.span);
            variants.push(EnumVariant { span, name: vname, kind });
            self.skip_comments();
            self.eat(TokenKind::Comma);
        }
        self.delimiter_depth -= 1;
        self.expect(TokenKind::CloseCurly)?;
        let span = self.span_from(start);
        Ok(Item { annotations: Vec::new(), span, kind: ItemKind::Enum(EnumDef { span, keyword_span: Some(start), name, generics, variants }) })
    }

    /// `newtype Name<gen>(WrappedType)`.
    fn parse_newtype(&mut self) -> PResult<Item> {
        let start = self.span();
        self.expect(TokenKind::Newtype)?;
        let name = self.expect_ident()?;
        let generics = self.parse_generics();
        self.expect(TokenKind::OpenParen)?;
        let wrapped = self.parse_type()?;
        self.expect(TokenKind::CloseParen)?;
        let span = self.span_from(start);
        Ok(Item { annotations: Vec::new(), span, kind: ItemKind::Newtype(NewtypeDef { span, name, generics, wrapped }) })
    }

    /// `deco init Name[(params)] { body }` — the `init` category keyword is
    /// required (rock dropped `deco wrap`); params are optional.
    fn parse_decorator(&mut self) -> PResult<Item> {
        let start = self.span();
        self.expect(TokenKind::Deco)?;
        self.skip_comments();
        // Consume the `init` contextual keyword.
        if matches!(self.kind(), TokenKind::Ident(n) if n.as_str() == "init") {
            self.advance();
        }
        let name = self.expect_ident()?;
        let params = if self.kind() == TokenKind::OpenParen {
            self.parse_params()?
        } else {
            Vec::new()
        };
        let body = self.parse_block()?;
        let span = self.span_from(start);
        Ok(Item { annotations: Vec::new(), span, kind: ItemKind::Decorator(DecoratorDef { span, name, params, body }) })
    }

    /// `global|const Name [: T] [= value]`.
    fn parse_global(&mut self, is_const: bool) -> PResult<Item> {
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
        // `is_guarded` is set by `parse_item` from a leading `@guarded` (the
        // annotation is consumed there before it reaches the item).
        Ok(Item { annotations: Vec::new(), span, kind: ItemKind::Global(GlobalDef { span, keyword_span: Some(start), name, ty, value, is_const, is_guarded: false }) })
    }

    /// `#if cond` / `#elseif cond` / `#else` / `#end` (cond is a bare ident).
    fn parse_compiler_directive(&mut self) -> PResult<Item> {
        let start = self.span();
        let tok = self.kind();
        self.advance();
        let directive = match tok {
            TokenKind::DirectiveIf => {
                let c = self.directive_condition();
                CompilerDirective::If(c)
            }
            TokenKind::DirectiveElseIf => {
                let c = self.directive_condition();
                CompilerDirective::ElseIf(c)
            }
            TokenKind::DirectiveElse => CompilerDirective::Else,
            _ => CompilerDirective::End,
        };
        let span = self.span_from(start);
        Ok(Item { annotations: Vec::new(), span, kind: ItemKind::CompilerDirective(directive) })
    }

    fn directive_condition(&mut self) -> Ustr {
        if let TokenKind::Ident(c) = self.kind() {
            self.advance();
            c
        } else {
            Ustr::from("")
        }
    }

    // ---- types ------------------------------------------------------------

    fn parse_type(&mut self) -> PResult<Idx<TypeExprData>> {
        // Leading comments (`-> /* c */ int`, `(/* c */ A, B)`, generic args).
        self.skip_comments();
        let start = self.span();
        let mut nullable = false;

        let kind = match self.kind() {
            TokenKind::OpenSquare => {
                // `[T]` array type
                self.advance();
                let inner = self.parse_type()?;
                self.expect(TokenKind::CloseSquare)?;
                TypeExprKind::Array(inner)
            }
            TokenKind::Bang => {
                // `![T]` native-array type. rock has no dedicated NativeArray
                // TypeExprKind — it carries it as `App{NativeArray, [T]}`, which
                // renders `(NativeArray T)`; mirror that exactly.
                let bang_span = self.span();
                self.advance();
                self.expect(TokenKind::OpenSquare)?;
                let inner = self.parse_type()?;
                self.expect(TokenKind::CloseSquare)?;
                TypeExprKind::App {
                    ctor: Ident { span: bang_span, name: Ustr::from("NativeArray") },
                    args: vec![inner],
                }
            }
            TokenKind::OpenParen => {
                // `()` Unit, `(T)` parenthesized T, `(A, B)` tuple, or
                // `(...) -> C` function type.
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
                    // `() -> T` has zero params; the tuple's elements otherwise.
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
                // `fn(A, B) -> C` function type; the `-> C` is optional and
                // defaults to Unit (`fn()` is a no-arg, unit-returning fn).
                self.advance();
                self.expect(TokenKind::OpenParen)?;
                let mut params = Vec::new();
                while self.kind() != TokenKind::CloseParen && !self.at_end() {
                    params.push(self.parse_type()?);
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
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
                match name.as_str() {
                    "any" => TypeExprKind::Any,
                    "string" => TypeExprKind::String,
                    "int" => TypeExprKind::Int,
                    "float" => TypeExprKind::Float,
                    "bool" => TypeExprKind::Bool,
                    _ => {
                        let ctor = Ident { span: ident_span, name };
                        // Generic application `Foo<A, B>`.
                        if self.kind() == TokenKind::LessThan {
                            self.advance();
                            let mut args = Vec::new();
                            while !matches!(
                                self.kind(),
                                TokenKind::GreaterThan | TokenKind::RightShift
                            ) && !self.at_end()
                            {
                                args.push(self.parse_type()?);
                                self.skip_comments(); // `Foo<int /* c */>`
                                if !self.eat(TokenKind::Comma) {
                                    break;
                                }
                            }
                            self.expect_generic_close()?;
                            TypeExprKind::App { ctor, args }
                        } else {
                            TypeExprKind::Ident(ctor)
                        }
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
        // Bare single-param function type `T -> U` (no parens around `T`). The
        // `fn(..)`/`(..)` forms consume their own `->`, so this only fires for
        // the bare spelling.
        if self.eat(TokenKind::Arrow) {
            let ret = self.parse_type()?;
            let fspan = self.span_from(start);
            return Ok(self.ast.ty(fspan, TypeExprKind::Function { params: vec![base], ret }, false));
        }
        Ok(base)
    }

    // ---- blocks & statements ---------------------------------------------

    fn parse_block(&mut self) -> PResult<Idx<BlockData>> {
        // A comment immediately before the `{` (e.g. `if cond /* c */ {`) is
        // skipped — it isn't a block statement and would otherwise wall here.
        self.skip_comments();
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
            let before = self.pos;
            match self.parse_statement() {
                Ok(stmt) => {
                    statements.push(stmt);
                    // Attach (consume) trailing comments on the same line as the
                    // statement so they don't become standalone comment statements.
                    self.consume_trailing_comments();
                    while self.eat(TokenKind::Semicolon) {}
                }
                Err(e) => {
                    // Statement-level recovery: a broken statement (e.g. a `let`
                    // with an empty RHS mid-edit) drops just that statement, not
                    // the whole `fn` body -- the lets before and after it survive
                    // so scope-based LSP features keep working on broken code.
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

    /// Consume comment tokens on the same source line as the just-parsed
    /// statement (matching rock's `collect_trailing_comments`): they attach to
    /// the statement rather than becoming standalone comment statements.
    fn consume_trailing_comments(&mut self) {
        if self.pos == 0 {
            return;
        }
        let stmt_line = self.tokens[self.pos - 1].span.line_range.0;
        while matches!(self.kind(), TokenKind::Comment(_))
            && self.span().line_range.0 == stmt_line
        {
            self.advance();
        }
    }

    fn parse_statement(&mut self) -> PResult<Idx<StatementData>> {
        let start = self.span();

        // `@global_write X = ..` prefix marks a guarded-global write. The flag
        // is semantic-only and absent from the s-expr; we record it on the
        // inner (assignment) statement so bone can enforce guarded-global writes.
        if self.kind() == TokenKind::At
            && matches!(self.peek_kind(), TokenKind::Ident(n) if n.as_str() == "global_write")
        {
            self.advance(); // `@`
            self.advance(); // `global_write`
            let stmt = self.parse_statement()?;
            self.ast.stmts[stmt].is_global_write = true;
            // Span the whole `@global_write ..` (rock includes the annotation
            // prefix in the statement span); the inner parse anchored at the
            // assignment.
            let span = self.merge_spans(start, self.ast.stmts[stmt].span);
            self.ast.stmts[stmt].span = span;
            return Ok(stmt);
        }

        // Contextual lifecycle-hook keywords: `on_resume`/`on_killed`/
        // `on_hot_reload` followed by `{` are statement-form hook blocks.
        // Elsewhere the same idents stay regular (the stdlib `on_hot_reload(f)`
        // call still works).
        if let TokenKind::Ident(name) = self.kind() {
            if matches!(name.as_str(), "on_resume" | "on_killed" | "on_hot_reload")
                && self.peek_kind() == TokenKind::OpenCurly
            {
                self.advance(); // hook keyword
                let body = self.parse_block()?;
                let span = self.span_from(start);
                return Ok(self.ast.stmt(
                    span,
                    StatementKind::LifecycleHook { keyword_span: Some(start), name, body },
                ));
            }
        }

        match self.kind() {
            TokenKind::Let => {
                self.advance();
                let binding = self.parse_binding()?;
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
                Ok(self.ast.stmt(span, StatementKind::Let(Let { span, keyword_span: Some(start), binding, ty, value })))
            }
            TokenKind::Return => {
                self.advance();
                // `return` with no value if a terminator / newline follows.
                let value = if self.stmt_terminator() {
                    None
                } else {
                    Some(self.parse_expr(0)?)
                };
                let span = self.span_from(start);
                Ok(self.ast.stmt(span, StatementKind::Return { keyword_span: Some(start), value }))
            }
            TokenKind::Break => {
                self.advance();
                Ok(self.ast.stmt(start, StatementKind::Break { keyword_span: Some(start) }))
            }
            TokenKind::Continue => {
                self.advance();
                Ok(self.ast.stmt(start, StatementKind::Continue { keyword_span: Some(start) }))
            }
            TokenKind::For => {
                self.advance();
                self.skip_comments(); // `for /* c */ v in /* c */ arr`
                let var = self.parse_binding()?;
                // Optional `: T` element type annotation.
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
                Ok(self.ast.stmt(span, StatementKind::For(For { span, keyword_span: Some(start), var, ty, iter, body })))
            }
            TokenKind::While => {
                self.advance();
                // `while let pat = value { .. }` binding form.
                self.skip_comments_in_expr();
                if self.kind() == TokenKind::Let {
                    self.advance();
                    let pattern = self.parse_pattern()?;
                    self.expect(TokenKind::Assign)?;
                    let value = self.with_allow_literals(false, |p| p.parse_expr(0))?;
                    let body = self.parse_block()?;
                    let span = self.span_from(start);
                    return Ok(self
                        .ast
                        .stmt(span, StatementKind::WhileLet(WhileLet { span, keyword_span: Some(start), pattern, value, body })));
                }
                let condition = self.with_allow_literals(false, |p| p.parse_expr(0))?;
                let body = self.parse_block()?;
                let span = self.span_from(start);
                Ok(self.ast.stmt(span, StatementKind::While(While { span, keyword_span: Some(start), condition, body })))
            }
            TokenKind::Loop => {
                // `loop { .. }` is `while true { .. }` (rock lowers it the same).
                self.advance();
                let condition = self.ast.expr(start, ExprKind::Bool(true));
                let body = self.parse_block()?;
                let span = self.span_from(start);
                Ok(self.ast.stmt(span, StatementKind::While(While { span, keyword_span: Some(start), condition, body })))
            }
            TokenKind::Defer => {
                self.advance();
                // `defer { ... }` is the block form; anything else is a bare
                // expression statement (`defer clear_attack(e)`) -- a leading
                // `(` parses as an ordinary parenthesized expression.
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

    /// Parse a `let` / `for` binding target: `x`, `(a, b)`, or `Name(x)`.
    fn parse_binding(&mut self) -> PResult<Binding> {
        if self.kind() == TokenKind::OpenParen {
            self.advance();
            let mut parts = Vec::new();
            while self.kind() != TokenKind::CloseParen && !self.at_end() {
                parts.push(self.parse_binding()?);
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
            self.expect(TokenKind::CloseParen)?;
            Ok(Binding::Tuple(parts))
        } else {
            let ident = self.expect_ident()?;
            if self.kind() == TokenKind::OpenParen {
                // newtype binding `Name(inner)`
                self.advance();
                let binding = self.expect_ident()?;
                self.expect(TokenKind::CloseParen)?;
                Ok(Binding::Newtype { name: ident, binding })
            } else {
                Ok(Binding::Ident(ident))
            }
        }
    }

    /// True when the current position ends a statement (closing brace,
    /// semicolon, EOF, a same-line trailing comment, or a preceding newline).
    fn stmt_terminator(&self) -> bool {
        // A same-line trailing comment (`return  // note`) ends the statement:
        // a value, if any, comes BEFORE the comment, so a comment in next-token
        // position means a value-less `return`. Without this, the comment +
        // following `}` get mis-parsed as the return value (dropping the item).
        matches!(
            self.kind(),
            TokenKind::CloseCurly | TokenKind::Semicolon | TokenKind::EOF | TokenKind::Comment(_)
        ) || self.at_end()
            || self.prev_trailing_newlines() > 0
    }

    // ---- expressions (Pratt) ---------------------------------------------

    fn parse_expr(&mut self, min_bp: u8) -> PResult<Idx<ExprData>> {
        let mut left = self.parse_prefix()?;

        loop {
            self.skip_comments_in_expr();
            let op = self.kind();

            // Type ascription `expr :: Type` is intercepted before the
            // precedence checks (as rock does), so it binds tighter than any
            // operator: `j :: float / k :: float` is `(j::float) / (k::float)`.
            // Qualified idents (`Color::Red`) were already consumed in primary.
            if op == TokenKind::DoubleColon {
                self.advance();
                let ty = self.parse_type()?;
                let span = self.node_span(left, self.prev_span());
                left = self.ast.expr(span, ExprKind::TypeAscription(left, ty));
                continue;
            }

            // Sigil macro call `name!(args)` — `!` before `(` enables extended
            // (type-shaped) arg parsing; renders as an ordinary call. A UFCS
            // receiver `recv.method!(args)` desugars to `method(recv, args)` at
            // parse time (eager, so the macro pass needs no UFCS knowledge).
            if op == TokenKind::Bang && self.peek_kind() == TokenKind::OpenParen {
                match self.ast.exprs[left].kind.clone() {
                    ExprKind::Ident(_) => {
                        self.advance(); // `!`
                        left = self.parse_sigil_call(left)?;
                        continue;
                    }
                    ExprKind::FieldAccess(recv, method) => {
                        self.advance(); // `!`
                        let recv_span = self.ast.exprs[recv].span;
                        let func = self.ast.expr(method.span, ExprKind::Ident(method));
                        let call = self.parse_sigil_call(func)?;
                        // Prepend the receiver as the first argument (UFCS).
                        if let ExprKind::FunctionCall { args, .. } = &mut self.ast.exprs[call].kind {
                            args.insert(0, FunctionCallArg { name: None, value: recv, is_splat: false });
                        }
                        // The UFCS call spans the receiver too (`recv.method!(..)`),
                        // matching rock — `parse_sigil_call` only spanned `method!(..)`.
                        let span = self.merge_spans(recv_span, self.ast.exprs[call].span);
                        self.ast.exprs[call].span = span;
                        left = call;
                        continue;
                    }
                    _ => {}
                }
            }

            // Postfix: call `(`, index `[`.
            if let Some((l_bp, ())) = op.postfix_bp() {
                if l_bp < min_bp {
                    break;
                }
                if op_starts_expr(op) && self.newline_breaks() {
                    break;
                }
                match op {
                    TokenKind::OpenParen => {
                        left = self.parse_call(left)?;
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
                    TokenKind::QuestionMark => {
                        let span = self.node_span(left, self.span());
                        self.advance();
                        left = self.ast.expr(span, ExprKind::QuestionMark(left));
                        continue;
                    }
                    TokenKind::PlusPlus => {
                        let span = self.node_span(left, self.span());
                        self.advance();
                        left = self.ast.expr(span, ExprKind::PostIncrement(left));
                        continue;
                    }
                    TokenKind::MinusMinus => {
                        let span = self.node_span(left, self.span());
                        self.advance();
                        left = self.ast.expr(span, ExprKind::PostDecrement(left));
                        continue;
                    }
                    TokenKind::QuestionDot => {
                        self.advance();
                        // `recv?.let { body }`: bind the unwrapped receiver to the
                        // receiver's ident name (else `value`) and run the block.
                        if self.kind() == TokenKind::Let {
                            self.advance();
                            let binding = match &self.ast.exprs[left].kind {
                                ExprKind::Ident(id) => id.clone(),
                                _ => Ident { span: self.ast.exprs[left].span, name: Ustr::from("value") },
                            };
                            let body = self.parse_block()?;
                            let span = self.node_span(left, self.prev_span());
                            left = self.ast.expr(
                                span,
                                ExprKind::SafeNavigationLet { receiver: left, binding, body },
                            );
                            continue;
                        }
                        let field = match self.kind() {
                            TokenKind::Ident(name) => {
                                let s = self.span();
                                self.advance();
                                Ident { span: s, name }
                            }
                            TokenKind::Int(num, _) => {
                                let s = self.span();
                                self.advance();
                                Ident { span: s, name: Ustr::from(&(num as usize).to_string()) }
                            }
                            // Other `?.` forms aren't in the subset.
                            _ => break,
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

            // Infix.
            if let Some((l_bp, r_bp)) = op.infix_bp() {
                if l_bp < min_bp {
                    break;
                }
                if op_starts_expr(op) && self.newline_breaks() {
                    break;
                }

                if op == TokenKind::Dot {
                    left = self.parse_dot(left)?;
                    continue;
                }

                if op == TokenKind::As {
                    // `expr as Type` — the RHS is a type, not an expression.
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

    /// rock's no-parens call: `foo a b` becomes `foo(a, b)` when `foo` is an
    /// ident/paren, the next token starts a value (not an operator), the callee
    /// and first arg share a line, and we are not already inside a no-parens
    /// arg list or a high-precedence context (`min_bp >= 50`). Returns the
    /// built call, or None if the form does not apply (cursor untouched).
    fn try_no_parens_call(&mut self, func: Idx<ExprData>, min_bp: u8) -> PResult<Option<Idx<ExprData>>> {
        if min_bp >= 50 || self.in_no_parens_args {
            return Ok(None);
        }
        if !matches!(self.ast.exprs[func].kind, ExprKind::Ident(_) | ExprKind::Paren(_)) {
            return Ok(None);
        }
        if self.prev_trailing_newlines() > 0 || !no_parens_arg_start(self.kind()) {
            return Ok(None);
        }

        let allow = self.allow_literals;
        let arg_min_bp = if min_bp >= 50 { 55 } else { 15 };
        self.in_no_parens_args = true;
        let mut args = Vec::new();
        while !self.at_end() {
            // Stop on terminators, statement keywords, or a fresh line.
            let stop = matches!(
                self.kind(),
                TokenKind::Comma
                    | TokenKind::CloseCurly
                    | TokenKind::CloseParen
                    | TokenKind::CloseSquare
                    | TokenKind::Semicolon
                    | TokenKind::EOF
                    | TokenKind::Return
                    | TokenKind::Let
                    | TokenKind::If
                    | TokenKind::For
                    | TokenKind::While
                    | TokenKind::Loop
                    | TokenKind::Break
                    | TokenKind::Continue
            ) || self.prev_trailing_newlines() > 0;
            if stop {
                break;
            }
            let is_splat = self.eat(TokenKind::TripleDot);
            let value = self.with_allow_literals(allow, |p| p.parse_expr(arg_min_bp))?;
            args.push(FunctionCallArg { name: None, value, is_splat });

            // Continue only if the next token can start another argument, on
            // the same line, outside delimiters, and isn't an operator.
            if !no_parens_arg_continue(self.kind())
                || self.prev_trailing_newlines() > 0
                || self.delimiter_depth > 0
                || no_parens_breaks_on_op(self.kind())
            {
                break;
            }
        }
        self.in_no_parens_args = false;

        if args.is_empty() {
            return Ok(None);
        }
        let span = self.node_span(func, self.prev_span());
        Ok(Some(self.ast.expr(
            span,
            ExprKind::FunctionCall { func, args, syntax: CallSyntax::NoParens },
        )))
    }

    /// rock's newline rule: a postfix/infix operator on a fresh line does
    /// not continue the expression, except inside `(`/`[` (where it does,
    /// unless we are also inside a `{ }` code block). The rule applies ONLY to
    /// operators that could also *start* a new expression (see `op_starts_expr`)
    /// -- rock only runs its newline-break check for those operand-start
    /// tokens, so an unambiguous binary op (`+`, `*`, `&&`) on a fresh line
    /// continues the expression regardless of indentation.
    fn newline_breaks(&self) -> bool {
        self.prev_trailing_newlines() > 0 && (self.delimiter_depth == 0 || self.brace_depth > 0)
    }

    fn build_infix(&mut self, left: Idx<ExprData>, op: TokenKind, op_span: Span, right: Idx<ExprData>) -> Idx<ExprData> {
        let span = self.node_span(left, self.ast.exprs[right].span);
        if op == TokenKind::Assign {
            return self.ast.expr(
                span,
                ExprKind::Assign {
                    target: left,
                    value: right,
                    compound_op: None,
                },
            );
        }
        // Compound assignment (`+= -= *= /=`) is a plain BinOp in rock — it
        // renders `(+= a b)` via the op's to_code, NOT `(= a b)` — so let it
        // fall through to the BinOp arm below rather than building an Assign.
        // `a $ b` is low-precedence application — `a(b)` (a normal call), the
        // lowest-binding operator. rock folds it to a call; skull must too.
        if op == TokenKind::Dollar {
            return self.ast.expr(
                span,
                ExprKind::FunctionCall {
                    func: left,
                    args: vec![FunctionCallArg { name: None, value: right, is_splat: false }],
                    syntax: CallSyntax::Normal,
                },
            );
        }
        // Overloadable arithmetic ops desugar to `__op_*(a, b)` calls with a
        // `Binary` render hint — exactly as rock's parser does.
        if let Some(op_fn) = op.overloadable_binop_fn() {
            let callee = self.ast.expr(
                op_span,
                ExprKind::Ident(Ident {
                    span: op_span,
                    name: Ustr::from(op_fn),
                }),
            );
            return self.ast.expr(
                span,
                ExprKind::FunctionCall {
                    func: callee,
                    args: vec![
                        FunctionCallArg { name: None, value: left, is_splat: false },
                        FunctionCallArg { name: None, value: right, is_splat: false },
                    ],
                    syntax: CallSyntax::Binary(op),
                },
            );
        }
        // Plain BinOp (comparisons, logic, bitwise, ranges, and compound
        // assignment — all render `(op left right)` via to_code). Compound
        // assignment is NOT a cook desugar site: bone `lower.rs` consumes the
        // raw `BinOp { op: AddAssign, .. }` directly.
        self.ast.expr(span, ExprKind::BinOp { left, op, right })
    }

    fn parse_dot(&mut self, left: Idx<ExprData>) -> PResult<Idx<ExprData>> {
        self.advance(); // consume `.`
        self.skip_comments();
        let field = match self.kind() {
            TokenKind::Ident(name) => {
                let span = self.span();
                self.advance();
                Ident { span, name }
            }
            TokenKind::Int(num, _) => {
                let span = self.span();
                self.advance();
                Ident { span, name: Ustr::from(&(num as usize).to_string()) }
            }
            _ => {
                // Keyword-as-member (`s.match`, `m.in`, ...).
                if let Some(kw) = self.kind().as_keyword_identifier() {
                    let span = self.span();
                    self.advance();
                    Ident { span, name: Ustr::from(kw) }
                } else {
                    return Err(self.err("field access `.` must be followed by an identifier or integer"));
                }
            }
        };
        let span = self.node_span(left, field.span);
        Ok(self.ast.expr(span, ExprKind::FieldAccess(left, field)))
    }

    fn parse_call(&mut self, func: Idx<ExprData>) -> PResult<Idx<ExprData>> {
        self.advance(); // consume `(`
        self.delimiter_depth += 1;
        let mut args = Vec::new();
        let mut seen_named = false;
        while self.kind() != TokenKind::CloseParen && !self.at_end() {
            self.skip_comments();
            let name = if matches!(self.kind(), TokenKind::Ident(_))
                && self.peek_kind() == TokenKind::Colon
            {
                let ident = self.expect_ident()?;
                self.expect(TokenKind::Colon)?;
                seen_named = true;
                Some(ident)
            } else if seen_named {
                return Err(self.err("positional argument after named argument is not allowed"));
            } else {
                None
            };
            // `f(...rest)` — a splat arg (carried as a flag, value = the inner
            // expr), matching rock; otherwise `...x` would parse as a `Spread`
            // expr and macro splat expansion couldn't recognize it.
            let is_splat = self.eat(TokenKind::TripleDot);
            let value = self.with_allow_literals(true, |p| p.parse_expr(0))?;
            args.push(FunctionCallArg { name, value, is_splat });
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.delimiter_depth -= 1;
        let end = self.expect(TokenKind::CloseParen)?;
        let span = self.node_span(func, end);
        Ok(self.ast.expr(
            span,
            ExprKind::FunctionCall { func, args, syntax: CallSyntax::Normal },
        ))
    }

    /// `@` trailing-lambda operator: `f(args) @ rhs` becomes
    /// `f(args, () => rhs)` and `f(args) @ { .. }` passes the block body
    /// directly. A non-call `left` becomes a call with the lambda as its only
    /// arg. Right-associative at bp 10. Mirrors rock's `At` transform; the
    /// resulting `FunctionCall` renders as a normal `(call ..)`.
    fn parse_at(&mut self, left: Idx<ExprData>) -> PResult<Idx<ExprData>> {
        self.advance(); // consume `@`

        // Lambda body: a `{ .. }` block is used directly; any other expression
        // is wrapped in a one-statement block (rock's create_lambda_from_expr).
        let body = if self.kind() == TokenKind::OpenCurly {
            self.parse_block()?
        } else {
            let rhs = self.parse_expr(10)?;
            let span = self.ast.exprs[rhs].span;
            let stmt = self.ast.stmt(span, StatementKind::Expression(rhs));
            self.ast.block(span, None, vec![stmt])
        };
        let body_span = self.ast.blocks[body].span;
        let lambda = self
            .ast
            .expr(body_span, ExprKind::Lambda { params: vec![], return_ty: None, body });
        let lambda_arg = FunctionCallArg { name: None, value: lambda, is_splat: false };

        // Append to an existing call, else wrap `left` in a new call.
        if matches!(self.ast.exprs[left].kind, ExprKind::FunctionCall { .. }) {
            // Extend the call's span through the `@ rhs` body (rock spans the
            // whole `f(..) @ rhs`, not just the original `f(..)`).
            let new_span = self.node_span(left, body_span);
            if let ExprKind::FunctionCall { args, syntax, .. } = &mut self.ast.exprs[left].kind {
                args.push(lambda_arg);
                *syntax = CallSyntax::At;
            }
            self.ast.exprs[left].span = new_span;
            Ok(left)
        } else {
            let span = self.node_span(left, body_span);
            Ok(self.ast.expr(
                span,
                ExprKind::FunctionCall { func: left, args: vec![lambda_arg], syntax: CallSyntax::At },
            ))
        }
    }

    fn parse_prefix(&mut self) -> PResult<Idx<ExprData>> {
        self.skip_comments();
        let start = self.span();

        // `(params) => body` lambda — lookahead distinguishes it from a
        // parenthesized expr / tuple. Suppressed in a match-arm guard so a
        // `(name) =>` doesn't eat the arm's `=>` (`docs/match-guard-parse-ambiguity.md`).
        if !self.arrow_lambda_blocked
            && self.kind() == TokenKind::OpenParen
            && self.looks_like_lambda_params()
        {
            self.advance(); // `(`
            let mut params = Vec::new();
            while self.kind() != TokenKind::CloseParen && !self.at_end() {
                let name = self.expect_ident()?;
                let ty = if self.eat(TokenKind::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                let span = self.span_from(name.span);
                params.push(LambdaParam { span, name, ty });
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
            self.expect(TokenKind::CloseParen)?;
            return self.finish_lambda(params, start);
        }

        // `![ ... ]` native array literal (`!` then `[`).
        if self.kind() == TokenKind::Bang && self.peek_kind() == TokenKind::OpenSquare {
            self.advance(); // `!`
            self.advance(); // `[`
            let items = self.parse_expr_list(TokenKind::CloseSquare)?;
            let end = self.expect(TokenKind::CloseSquare)?;
            let span = self.merge_spans(start, end);
            return Ok(self.ast.expr(span, ExprKind::NativeArrayLiteral(items)));
        }

        // Leading `.ident` is a sibling-field reference (`current: int = .max`).
        // Only valid in a struct field default; the type-checker enforces context.
        if self.kind() == TokenKind::Dot && matches!(self.peek_kind(), TokenKind::Ident(_)) {
            self.advance(); // `.`
            let ident = self.expect_ident()?;
            let span = self.span_from(start);
            return Ok(self.ast.expr(span, ExprKind::SiblingFieldRef(ident)));
        }

        let kind = self.kind();

        // Prefix unary operators.
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

        // Spread `...expr` (in array literals / call args).
        if kind == TokenKind::TripleDot {
            self.advance();
            let operand = self.parse_expr(0)?;
            // End at the operand, NOT the current position: parsing `...y` before
            // a closing `]`/`)` runs the infix loop's `skip_comments_in_expr`,
            // which consumes a trailing `// comment` (the closer counts as `is_op`).
            // A `span_from(start)` would then swallow that comment into the spread
            // span, and the formatter would emit it twice (verbatim slice + the
            // re-applied trailing comment).
            let span = self.merge_spans(start, self.ast.exprs[operand].span);
            return Ok(self.ast.expr(span, ExprKind::Spread(operand)));
        }

        // `&name` function reference (prefix `&` before an ident).
        if kind == TokenKind::BitAnd {
            self.advance();
            let name = self.expect_ident()?;
            let span = self.span_from(start);
            return Ok(self.ast.expr(span, ExprKind::FunctionRef(name)));
        }

        // `^name` anaphoric ident (prefix `^` before an ident; `^` is otherwise
        // infix xor, illegal as a prefix).
        if kind == TokenKind::BitXor {
            self.advance();
            let name = self.expect_ident()?;
            let span = self.span_from(start);
            // Not a cook desugar site: bone `lower.rs` resolves the raw
            // `AnaphoricIdent` as a caller-scope ident read directly.
            return Ok(self.ast.expr(span, ExprKind::AnaphoricIdent(name)));
        }

        // `?` / `?name` typed hole in value position. At expression start `?`
        // can only be a hole (postfix unwrap / infix `??` are handled in the
        // pratt loop, which always has a left operand). The name must be
        // byte-adjacent (`?foo`); any gap means an anonymous hole followed by a
        // separate expression — mirrors rock's `exprs.rs` hole arm.
        if kind == TokenKind::QuestionMark {
            self.advance();
            let id = self.gen_hole_id();
            let name = if let TokenKind::Ident(n) = self.kind() {
                if self.prev_span().offset_range.1 == self.span().offset_range.0 {
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
            TokenKind::Int(num, text) => {
                self.advance();
                Ok(self.ast.expr(start, ExprKind::Int(num, text)))
            }
            TokenKind::Float(f) => {
                self.advance();
                Ok(self.ast.expr(start, ExprKind::Float(f)))
            }
            TokenKind::String(s) => {
                let span = self.span();
                self.advance();
                self.parse_interpolated(s, span)
            }
            TokenKind::RawString(s) => {
                // Raw strings never interpolate.
                self.advance();
                Ok(self.ast.expr(start, ExprKind::String(s)))
            }
            TokenKind::True => {
                self.advance();
                Ok(self.ast.expr(start, ExprKind::Bool(true)))
            }
            TokenKind::False => {
                self.advance();
                Ok(self.ast.expr(start, ExprKind::Bool(false)))
            }
            TokenKind::Null => {
                self.advance();
                Ok(self.ast.expr(start, ExprKind::Null))
            }
            TokenKind::Ident(name) => {
                self.advance();
                let base = Ident { span: start, name };
                // `x => body` single-parameter lambda. Suppressed in a match-arm
                // guard so the arm's `=>` survives (`docs/match-guard-parse-ambiguity.md`).
                if !self.arrow_lambda_blocked && self.kind() == TokenKind::FatArrow {
                    return self.finish_lambda(vec![LambdaParam { span: base.span, name: base, ty: None }], start);
                }
                // `Base::Member` is a qualified ident ONLY when the member
                // starts uppercase (enum-variant convention). A lowercase
                // member (`value :: int`) is type ascription, handled by the
                // infix `::` branch — so fall through to a plain ident here.
                if self.kind() == TokenKind::DoubleColon && self.peek_is_uppercase_ident() {
                    self.advance(); // `::`
                    let member = self.expect_ident()?;
                    if self.struct_literal_follows() {
                        let fields = self.parse_struct_fields()?;
                        let span = self.span_from(start);
                        return Ok(self.ast.expr(
                            span,
                            ExprKind::EnumVariantRecord {
                                enum_name: base,
                                variant_name: member,
                                fields,
                            },
                        ));
                    }
                    let span = self.span_from(start);
                    Ok(self.ast.expr(span, ExprKind::QualifiedIdent { base, member }))
                } else if self.struct_literal_follows() {
                    let fields = self.parse_struct_fields()?;
                    let span = self.span_from(start);
                    Ok(self.ast.expr(span, ExprKind::StructLiteral { name: base, fields }))
                } else {
                    Ok(self.ast.expr(start, ExprKind::Ident(base)))
                }
            }
            TokenKind::Match => self.parse_match(),
            TokenKind::OpenParen => {
                self.advance();
                self.delimiter_depth += 1;
                if self.eat(TokenKind::CloseParen) {
                    self.delimiter_depth -= 1;
                    // Span the whole `()`, not just the open paren.
                    return Ok(self.ast.expr(self.span_from(start), ExprKind::Unit));
                }
                let first = self.with_allow_literals(true, |p| p.parse_expr(0))?;
                // `(a, b, ...)` is a tuple; `(a)` is a parenthesized expr.
                if self.kind() == TokenKind::Comma {
                    let mut items = vec![first];
                    while self.eat(TokenKind::Comma) {
                        if self.kind() == TokenKind::CloseParen {
                            break;
                        }
                        items.push(self.with_allow_literals(true, |p| p.parse_expr(0))?);
                    }
                    self.delimiter_depth -= 1;
                    let end = self.expect(TokenKind::CloseParen)?;
                    let span = self.merge_spans(start, end);
                    Ok(self.ast.expr(span, ExprKind::Tuple(items)))
                } else {
                    self.delimiter_depth -= 1;
                    let end = self.expect(TokenKind::CloseParen)?;
                    let span = self.merge_spans(start, end);
                    Ok(self.ast.expr(span, ExprKind::Paren(first)))
                }
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
            TokenKind::Return => {
                self.advance();
                let v = if self.expr_terminates() {
                    None
                } else {
                    Some(self.parse_expr(0)?)
                };
                Ok(self.ast.expr(self.span_from(start), ExprKind::ReturnExpr(v)))
            }
            TokenKind::Break => {
                self.advance();
                Ok(self.ast.expr(start, ExprKind::BreakExpr))
            }
            TokenKind::Continue => {
                self.advance();
                Ok(self.ast.expr(start, ExprKind::ContinueExpr))
            }
            TokenKind::Yield => {
                self.advance();
                if self.eat(TokenKind::Mul) {
                    // `yield* coro` delegation.
                    let inner = self.parse_expr(0)?;
                    return Ok(self.ast.expr(self.span_from(start), ExprKind::YieldDelegate(inner)));
                }
                let v = if self.expr_terminates() {
                    None
                } else {
                    Some(self.parse_expr(0)?)
                };
                Ok(self.ast.expr(self.span_from(start), ExprKind::Yield(v)))
            }
            other => Err(self.err(format!("unexpected token in expression: {:?}", other))),
        }
    }

    /// True when an optional trailing expression (after `return` / `yield`)
    /// is absent: a closing delimiter, separator, newline, or end follows.
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
        let mut items = Vec::new();
        while self.kind() != close && !self.at_end() {
            self.skip_comments();
            if self.kind() == close {
                break;
            }
            items.push(self.with_allow_literals(true, |p| p.parse_expr(0))?);
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.delimiter_depth -= 1;
        Ok(items)
    }

    /// Split a string literal into literal/expr parts on `{...}`, mirroring
    /// rock's algorithm: `{{`/`}}` are escaped braces; `{expr:fmt}` embeds an
    /// expression (the format spec is parsed-and-discarded — it does not
    /// affect s-exprs); a `{...}` whose body fails to parse falls back to
    /// literal text. A lone literal collapses to `ExprKind::String`.
    fn parse_interpolated(&mut self, raw: Ustr, span: Span) -> PResult<Idx<ExprData>> {
        let file = span.file;
        let s = raw.as_str();
        let mut parts: Vec<InterpolatedPart> = Vec::new();
        let mut buf = String::new();
        let mut chars = s.chars().peekable();
        // Position within the string content, tracked so embedded expressions
        // get file-relative spans (offset/line/col) rather than substring-local
        // ones. Mirrors rock's `parse_interpolated` math exactly so the A/B
        // matches; `offset` is byte-based (1-byte ASCII for `"`/`{`/`}`).
        let mut char_pos = 0usize;
        let mut byte_pos = 0usize;

        while let Some(c) = chars.next() {
            if c == '{' && chars.peek() == Some(&'{') {
                chars.next();
                buf.push('{');
                char_pos += 2;
                byte_pos += 2;
                continue;
            }
            if c == '}' && chars.peek() == Some(&'}') {
                chars.next();
                buf.push('}');
                char_pos += 2;
                byte_pos += 2;
                continue;
            }
            if c == '{' {
                if !buf.is_empty() {
                    parts.push(InterpolatedPart::Literal(Ustr::from(&buf)));
                    buf.clear();
                }
                // `+1` opening quote, `+1` the `{` itself.
                let expr_start_pos = char_pos + 1;
                let expr_start_offset = span.offset_range.0 + 1 + byte_pos + 1;
                let mut expr_str = String::new();
                let mut format_str = String::new();
                let mut in_format = false;
                let mut spec_depth: i32 = 0;
                let mut inner_char_count = 0usize;
                let mut inner_byte_count = 0usize;
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    inner_char_count += 1;
                    inner_byte_count += nc.len_utf8();
                    if nc == '}' && spec_depth == 0 {
                        break;
                    }
                    if nc == ':' && !in_format {
                        in_format = true;
                        continue;
                    }
                    if in_format {
                        if nc == '{' {
                            spec_depth += 1;
                        } else if nc == '}' {
                            spec_depth -= 1;
                        }
                        format_str.push(nc);
                    } else {
                        expr_str.push(nc);
                    }
                }
                // Line/col of the expression start: walk the content up to the
                // `{`, counting newlines (interpolated strings may be multi-line).
                let mut lines_seen = 0usize;
                let mut col_in_line = 0usize;
                for ch in s.chars().take(char_pos) {
                    if ch == '\n' {
                        lines_seen += 1;
                        col_in_line = 0;
                    } else {
                        col_in_line += 1;
                    }
                }
                let expr_line = span.line_range.0 + lines_seen;
                let expr_col = if lines_seen == 0 {
                    span.col_range.0 + 1 + expr_start_pos
                } else {
                    col_in_line + 1
                };
                match self.parse_interp_subexpr(
                    &expr_str,
                    file,
                    expr_start_offset,
                    expr_line,
                    expr_col,
                ) {
                    Ok(e) => {
                        // Parse the `:spec` into a FormatSpec (incl. dynamic
                        // `{w}`/`{p}` sub-expressions). The spec never renders in
                        // the s-expr, but capturing it keeps those embedded
                        // exprs from being lost. Positions are best-effort
                        // (spec spans feed diagnostics only, not the oracle).
                        let spec = if format_str.is_empty() {
                            None
                        } else {
                            let fmt_offset = expr_start_offset + expr_str.len() + 1;
                            let fmt_col = expr_col + expr_str.chars().count() + 1;
                            Some(self.parse_format_spec(&format_str, file, fmt_offset, expr_line, fmt_col)?)
                        };
                        parts.push(InterpolatedPart::Expr(e, spec));
                    }
                    Err(_) => {
                        // Couldn't parse the embedded expression — emit the
                        // braces and contents verbatim, like rock.
                        buf.push('{');
                        buf.push_str(&expr_str);
                        if !format_str.is_empty() {
                            buf.push(':');
                            buf.push_str(&format_str);
                        }
                        buf.push('}');
                    }
                }
                char_pos += inner_char_count + 1; // +1 for the opening `{`
                byte_pos += inner_byte_count + 1;
                continue;
            }
            buf.push(c);
            char_pos += 1;
            byte_pos += c.len_utf8();
        }
        if !buf.is_empty() {
            parts.push(InterpolatedPart::Literal(Ustr::from(&buf)));
        }

        let kind = if parts.len() == 1 {
            match &parts[0] {
                InterpolatedPart::Literal(lit) => ExprKind::String(*lit),
                _ => ExprKind::InterpolatedString(parts),
            }
        } else {
            ExprKind::InterpolatedString(parts)
        };
        Ok(self.ast.expr(span, kind))
    }

    /// Parse one embedded interpolation expression into the *same* arena by
    /// swapping the token stream/cursor for the sub-tokens, parsing, then
    /// restoring. Reuses the full Pratt parser so precedence is identical.
    fn parse_interp_subexpr(
        &mut self,
        src: &str,
        file: Ustr,
        offset: usize,
        line: usize,
        column: usize,
    ) -> PResult<Idx<ExprData>> {
        // `new_with_offset` seeds the sub-tokenizer with the embedded
        // expression's position in the enclosing file, so its tokens carry
        // file-relative spans rather than substring-local ones.
        let tokens = Tokenizer::new_with_offset(src, file, offset, line, column).tokenize()?;
        let saved_tokens = std::mem::replace(&mut self.tokens, tokens);
        let saved_pos = std::mem::replace(&mut self.pos, 0);
        let saved_eof = self.eof_span;
        let saved_delim = self.delimiter_depth;
        let saved_brace = self.brace_depth;
        self.eof_span = self.tokens.last().map(|t| t.span).unwrap_or_else(Span::unknown);
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

    /// Parse the `:spec` of `{expr:spec}` into a `FormatSpec`, mirroring rock's
    /// `parse_format_specifier`. `fmt_*` describe the position of the first
    /// char of `format_str` in the enclosing file (used only for the spans of
    /// dynamic `{w}`/`{p}` sub-expressions). Errors on a malformed spec, like
    /// rock — the spec is parsed-and-stored, never silently dropped.
    fn parse_format_spec(
        &mut self,
        format_str: &str,
        file: Ustr,
        fmt_offset: usize,
        fmt_line: usize,
        fmt_col: usize,
    ) -> PResult<FormatSpec> {
        // Grammar (Python-like):
        // [[fill]align][sign][#][0][width|{expr}][grouping][.precision|.{expr}][type]
        let mut spec = FormatSpec::new();
        let mut chars = format_str.chars().peekable();

        // fill + align (together) or bare align.
        if let Some(&first) = chars.peek() {
            if chars.clone().nth(1).map_or(false, |c| matches!(c, '<' | '>' | '^' | '=')) {
                spec.fill = Some(first);
                chars.next();
                if let Some(align) = chars.next() {
                    spec.alignment = Some(align);
                }
            } else if matches!(first, '<' | '>' | '^' | '=') {
                spec.alignment = Some(first);
                chars.next();
            }
        }
        // sign
        if let Some(&c) = chars.peek() {
            if matches!(c, '+' | '-' | ' ') {
                spec.sign = Some(c);
                chars.next();
            }
        }
        // alternate `#`
        if chars.peek() == Some(&'#') {
            spec.alternate = true;
            chars.next();
        }
        // zero pad `0`
        if chars.peek() == Some(&'0') {
            spec.zero_pad = true;
            chars.next();
        }
        // width: digits or `{expr}`
        if chars.peek() == Some(&'{') {
            let (off, line, col) = self.dyn_expr_pos(format_str, &chars, fmt_offset, fmt_line, fmt_col);
            chars.next(); // `{`
            let src = take_balanced_brace_body(&mut chars, format_str)?;
            spec.width_expr = Some(self.parse_interp_subexpr(&src, file, off, line, col)?);
        } else {
            let mut w = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_digit() {
                    w.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            if !w.is_empty() {
                spec.width = w.parse().ok();
            }
        }
        // grouping `,` / `_`
        if let Some(&c) = chars.peek() {
            if c == ',' || c == '_' {
                spec.grouping = Some(c);
                chars.next();
            }
        }
        // precision: `.digits` or `.{expr}`
        if chars.peek() == Some(&'.') {
            chars.next(); // `.`
            if chars.peek() == Some(&'{') {
                let (off, line, col) = self.dyn_expr_pos(format_str, &chars, fmt_offset, fmt_line, fmt_col);
                chars.next(); // `{`
                let src = take_balanced_brace_body(&mut chars, format_str)?;
                spec.precision_expr = Some(self.parse_interp_subexpr(&src, file, off, line, col)?);
            } else {
                let mut p = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_digit() {
                        p.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if !p.is_empty() {
                    spec.precision = p.parse().ok();
                }
            }
        }
        // format type
        if let Some(&c) = chars.peek() {
            if matches!(
                c,
                'b' | 'c' | 'd' | 'e' | 'E' | 'f' | 'F' | 'g' | 'G' | 'n' | 'o' | 's' | 'x' | 'X' | '%'
            ) {
                spec.format_type = Some(c);
                chars.next();
            }
        }
        if chars.peek().is_some() {
            return Err(self.err(format!("Invalid format specifier: '{}'", format_str)));
        }
        Ok(spec)
    }

    /// File position of the `{` a dynamic width/precision placeholder begins at,
    /// given the remaining `chars` cursor into `format_str`. Best-effort: the
    /// resulting spans only feed diagnostics for the sub-expression.
    fn dyn_expr_pos(
        &self,
        format_str: &str,
        chars: &std::iter::Peekable<std::str::Chars>,
        fmt_offset: usize,
        fmt_line: usize,
        fmt_col: usize,
    ) -> (usize, usize, usize) {
        let total = format_str.chars().count();
        let brace_idx = total - chars.clone().count();
        let brace_byte = format_str
            .char_indices()
            .nth(brace_idx)
            .map(|(b, _)| b)
            .unwrap_or(format_str.len());
        // +1 to step past the `{` onto the inner expression's first char.
        (fmt_offset + brace_byte + 1, fmt_line, fmt_col + brace_idx + 1)
    }

    /// `Name {` opens a struct literal only when literals are allowed here
    /// and the `{` is on the same line (matching rock's guard).
    fn struct_literal_follows(&self) -> bool {
        self.allow_literals
            && self.kind() == TokenKind::OpenCurly
            && self.prev_trailing_newlines() == 0
    }

    /// Parse `{ f1: v1, f2 + f3: v, f4 }` field list (shared by struct
    /// literals and enum record-variant construction).
    fn parse_struct_fields(&mut self) -> PResult<Vec<(Vec<Ident>, Idx<ExprData>)>> {
        self.expect(TokenKind::OpenCurly)?;
        self.delimiter_depth += 1;
        let mut fields = Vec::new();
        while self.kind() != TokenKind::CloseCurly && !self.at_end() {
            self.skip_comments();
            if self.kind() == TokenKind::CloseCurly {
                break;
            }
            let mut names = vec![self.expect_ident()?];
            while self.kind() == TokenKind::Plus {
                self.advance();
                names.push(self.expect_ident()?);
            }
            let value = if self.eat(TokenKind::Colon) {
                self.with_allow_literals(true, |p| p.parse_expr(0))?
            } else {
                // Shorthand `{ x }` -> `x: x`.
                let n = names[0].clone();
                self.ast.expr(n.span, ExprKind::Ident(n))
            };
            fields.push((names, value));
            self.skip_comments();
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.skip_comments();
        self.delimiter_depth -= 1;
        self.expect(TokenKind::CloseCurly)?;
        Ok(fields)
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
                // Block the bare-ident / paren-form lambda commits so the guard
                // stops at the arm's `=>` (`docs/match-guard-parse-ambiguity.md`).
                let saved = std::mem::replace(&mut self.arrow_lambda_blocked, true);
                let g = self.with_allow_literals(false, |p| p.parse_expr(0));
                self.arrow_lambda_blocked = saved;
                Some(g?)
            } else {
                None
            };
            self.skip_comments(); // `pat /* c */ =>`
            self.expect(TokenKind::FatArrow)?;
            // Arm body: a block, a bare control-flow statement, or a single
            // expression — the last two are wrapped in a block, as rock does.
            let body = if self.kind() == TokenKind::OpenCurly {
                self.parse_block()?
            } else {
                let stmt_span = self.span();
                let stmt = match self.kind() {
                    TokenKind::Break | TokenKind::Continue | TokenKind::Return => {
                        self.parse_statement()?
                    }
                    _ => {
                        let e = self.with_allow_literals(false, |p| p.parse_expr(0))?;
                        let s = self.span_from(stmt_span);
                        self.ast.stmt(s, StatementKind::Expression(e))
                    }
                };
                let body_span = self.span_from(stmt_span);
                self.ast.block(body_span, None, vec![stmt])
            };
            arms.push(MatchArm { pattern, guard, body });
            self.eat(TokenKind::Comma);
        }
        self.brace_depth -= 1;
        self.expect(TokenKind::CloseCurly)?;
        let span = self.span_from(start);
        Ok(self.ast.expr(span, ExprKind::Match { scrutinee, arms }))
    }

    /// Top-level match pattern: a single pattern or an or-pattern `A | B | C`.
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

    fn parse_pattern(&mut self) -> PResult<Pattern> {
        // Literal patterns (and ranges starting with a literal).
        // Open-ended range with no start: `..N` / `..=N`.
        if matches!(self.kind(), TokenKind::DoubleDot) {
            self.advance();
            let inclusive = self.eat(TokenKind::Assign);
            let end = self.pattern_literal_expr();
            return Ok(Pattern::Range { start: None, end, inclusive });
        }
        if let Some(lit) = self.pattern_literal_expr() {
            if matches!(self.kind(), TokenKind::DoubleDot) {
                self.advance();
                // `..=` (inclusive) lexes as `DoubleDot` + `Assign`. The end is
                // optional: `N..` (end-less) yields `end: None`.
                let inclusive = self.eat(TokenKind::Assign);
                let end = self.pattern_literal_expr();
                return Ok(Pattern::Range { start: Some(lit), end, inclusive });
            }
            return Ok(Pattern::Literal(lit));
        }
        match self.kind() {
            TokenKind::OpenParen => {
                self.advance();
                let mut ps = Vec::new();
                while self.kind() != TokenKind::CloseParen && !self.at_end() {
                    ps.push(self.parse_pattern()?);
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(TokenKind::CloseParen)?;
                Ok(Pattern::Tuple(ps))
            }
            TokenKind::Ident(name) if name.as_str() == "_" => {
                self.advance();
                Ok(Pattern::Wildcard)
            }
            TokenKind::Ident(name) => {
                let ident_span = self.span();
                self.advance();
                let ident = Ident { span: ident_span, name };
                // Qualified `Enum::Variant`.
                let (enum_name, variant) = if self.kind() == TokenKind::DoubleColon
                    && matches!(self.peek_kind(), TokenKind::Ident(_))
                {
                    self.advance();
                    (Some(ident), self.expect_ident()?)
                } else {
                    (None, ident)
                };
                if self.kind() == TokenKind::OpenParen {
                    self.advance();
                    let mut args = Vec::new();
                    while self.kind() != TokenKind::CloseParen && !self.at_end() {
                        args.push(self.parse_pattern()?);
                        if !self.eat(TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(TokenKind::CloseParen)?;
                    Ok(Pattern::EnumVariant { enum_name, variant_name: variant, args: Some(args) })
                } else if self.kind() == TokenKind::OpenCurly {
                    let (fields, rest) = self.parse_pattern_record_fields()?;
                    Ok(Pattern::Record { enum_name, name: Some(variant), fields, rest })
                } else if enum_name.is_some() {
                    Ok(Pattern::EnumVariant { enum_name, variant_name: variant, args: None })
                } else {
                    Ok(Pattern::Ident(variant))
                }
            }
            other => Err(self.err(format!("unsupported pattern: {:?}", other))),
        }
    }

    /// `{ field: pat, other, .. }` record-pattern fields. A bare `field`
    /// binds `field` to a same-named pattern.
    fn parse_pattern_record_fields(&mut self) -> PResult<(Vec<(Ident, Pattern)>, bool)> {
        self.expect(TokenKind::OpenCurly)?;
        let mut fields = Vec::new();
        let mut rest = false;
        while self.kind() != TokenKind::CloseCurly && !self.at_end() {
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
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::CloseCurly)?;
        Ok((fields, rest))
    }

    /// Parse a literal expression usable in a pattern (int/float/string/
    /// bool/null), or `None` if the current token isn't a literal.
    fn pattern_literal_expr(&mut self) -> Option<Idx<ExprData>> {
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

    /// True if the tokens from the current `(` form `(ident, ...) =>` — a
    /// lambda parameter list rather than a parenthesized expr / tuple.
    fn looks_like_lambda_params(&self) -> bool {
        // Scan the balanced `( .. )` from the current `(` -- params may carry
        // `: Type` annotations (incl. nested `(`/`[`) -- then a lambda is
        // distinguished from a parenthesized expr / tuple by a trailing `=>`.
        // In expression position only a lambda has `(...) =>` (match patterns
        // are parsed elsewhere), so the `=>` check is decisive.
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

    /// Consume `=> body` after the params and build a lambda. The body is an
    /// explicit block or a single expression wrapped in a block (as rock does).
    fn finish_lambda(&mut self, params: Vec<LambdaParam>, start: Span) -> PResult<Idx<ExprData>> {
        self.expect(TokenKind::FatArrow)?;
        let body = if self.kind() == TokenKind::OpenCurly {
            self.parse_block()?
        } else {
            // Arrow body `=> expr` becomes a block with a single `return expr`
            // (rock's parse_lambda_body), so the s-expr matches. The synthetic
            // stmt + block span the WHOLE body expr (`x * 2`), not just its
            // first token — `self.span()` here is the first token, so take the
            // parsed expr's span after the fact (as the `@`-lambda path does).
            let e = self.with_allow_literals(true, |p| p.parse_expr(0))?;
            let body_span = self.ast.exprs[e].span;
            let stmt = self.ast.stmt(body_span, StatementKind::Return { keyword_span: None, value: Some(e) });
            self.ast.block(body_span, None, vec![stmt])
        };
        let span = self.span_from(start);
        Ok(self.ast.expr(span, ExprKind::Lambda { params, return_ty: None, body }))
    }

    /// True if the if-condition (scanning up to the block `{` at depth 0)
    /// contains a top-level `let` — i.e. it's an if-let-chain.
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
            if self.kind() == TokenKind::If {
                self.expect(TokenKind::If)?;
                let cond = self.with_allow_literals(false, |p| p.parse_expr(0))?;
                let body = self.parse_block()?;
                branches.push((cond, body));
            } else {
                else_branch = Some(self.parse_block()?);
                break;
            }
        }
        let span = self.span_from(start);
        Ok(self.ast.expr(span, ExprKind::If { branches, else_branch }))
    }

    /// `if [let pat = value | cond] [&& ..]* { .. } [else { .. }]`. Entered with
    /// the current token on the first `let`. A single binding renders as
    /// `IfLet`; multiple clauses as `IfLetChain` (mirrors rock).
    fn parse_if_let(&mut self, start: Span) -> PResult<Idx<ExprData>> {
        let mut clauses: Vec<IfLetClause> = Vec::new();
        loop {
            self.skip_comments_in_expr();
            if self.kind() == TokenKind::Let {
                self.advance();
                let pattern = self.parse_pattern()?;
                self.expect(TokenKind::Assign)?;
                // Stop before `&&` (And has l_bp=15); `{` is a block, not a lit.
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
        // Single binding -> plain IfLet (rock does the same, no desugar).
        if clauses.len() == 1 {
            if let IfLetClause::Binding { pattern, value } = clauses.remove(0) {
                return Ok(self.ast.expr(
                    span,
                    ExprKind::IfLet { pattern, value, then_block, else_block },
                ));
            }
        }
        Ok(self.ast.expr(span, ExprKind::IfLetChain { clauses, then_block, else_block }))
    }

    /// Span covering an expr child through an end token's span.
    fn node_span(&self, left: Idx<ExprData>, end: Span) -> Span {
        self.merge_spans(self.ast.exprs[left].span, end)
    }

    fn skip_comments_in_expr(&mut self) {
        // Only skip a comment if an operator/continuation follows, matching
        // rock's `skip_comments_if_followed_by_match`. Conservative: skip
        // when the token after the comment continues the expression.
        while matches!(self.kind(), TokenKind::Comment(_)) {
            let next = self.peek_kind();
            // A fresh-line expression-starter after the comment (`@` decorator /
            // `@global_write`, a unary `-`, ...) begins a NEW statement/item, not
            // a continuation of this expression -- the postfix/infix loop breaks
            // on it via `op_starts_expr && newline_breaks`, so stop here too and
            // leave the standalone comment for the item/statement loop to capture
            // instead of silently consuming it (widening the expression span).
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
            let continues = next.is_op()
                || matches!(
                    next,
                    TokenKind::OpenParen | TokenKind::OpenSquare | TokenKind::Dot
                );
            if continues {
                self.advance();
            } else {
                break;
            }
        }
    }
}

/// Operators that can ALSO begin a new expression, so a newline before them
/// signals a fresh statement rather than a continuation. rock applies its
/// newline-break rule only to this operand-start set (`exprs.rs` operand list:
/// `Minus`, `Bang`, `++`, `--`, `(`, `[`, `{`); unary `Minus`/`++`/`--` reach
/// skull's loop as infix/postfix candidates). Unambiguous binary operators
/// (`+`, `*`, `&&`, comparisons) are not here, so they continue across lines.
/// Consume a brace-balanced `{...}` body (cursor positioned just after the
/// opening `{`), returning the inner text. Mirror of rock's
/// `take_balanced_brace_body`. Errors on an unmatched `{`.
fn take_balanced_brace_body(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    full_spec: &str,
) -> PResult<String> {
    let mut body = String::new();
    let mut depth: usize = 1;
    for c in chars.by_ref() {
        if c == '{' {
            depth += 1;
            body.push(c);
        } else if c == '}' {
            depth -= 1;
            if depth == 0 {
                return Ok(body);
            }
            body.push(c);
        } else {
            body.push(c);
        }
    }
    Err(CompilerError::new_with_label(
        Span::unknown(),
        format!("unbalanced `{{` in format specifier: '{}' (expected matching `}}`)", full_spec),
    ))
}

fn op_starts_expr(op: TokenKind) -> bool {
    matches!(
        op,
        TokenKind::Minus
            | TokenKind::PlusPlus
            | TokenKind::MinusMinus
            | TokenKind::OpenParen
            | TokenKind::OpenSquare
            // `@` on a fresh line starts a new statement (`@global_write X = ..`,
            // or the next item's annotation), never an `expr @ lambda` glued to
            // the previous line. All real `@`-operator uses are same-line.
            | TokenKind::At
    )
}

/// Tokens that may begin the FIRST argument of a no-parens call. Mirrors
/// rock's `next_is_likely_call` minus `OpenSquare` (a postfix index, which
/// rock excludes via `!is_postfix_op`).
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

/// Tokens that can begin a FOLLOW-ON no-parens argument (rock's
/// `current_token_can_start_expr`).
fn no_parens_arg_continue(op: TokenKind) -> bool {
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
            | TokenKind::OpenParen
            | TokenKind::OpenSquare
            | TokenKind::OpenCurly
            | TokenKind::Minus
            | TokenKind::Bang
            | TokenKind::PlusPlus
            | TokenKind::MinusMinus
    )
}

/// Operators/separators that end a no-parens argument run rather than start a
/// new argument (so `foo a - b` is `foo(a) - b`, not `foo(a, -b)`).
fn no_parens_breaks_on_op(op: TokenKind) -> bool {
    matches!(
        op,
        TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::Mul
            | TokenKind::Div
            | TokenKind::Equals
            | TokenKind::NotEquals
            | TokenKind::LessThan
            | TokenKind::GreaterThan
            | TokenKind::And
            | TokenKind::Or
            | TokenKind::Assign
            | TokenKind::Dot
            | TokenKind::Dollar
            | TokenKind::At
    )
}
