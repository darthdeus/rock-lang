use crate::prelude::*;

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    pub trailing_newlines: usize,
}

/// Classify lines in `contents` into code/comment/blank using the actual
/// token stream, then return a `CompileStats` for that file. Single
/// source of truth on what's a comment vs. a string literal — replaces
/// the earlier byte-walker which mis-classified `//` and `/*` inside
/// strings (regression test: `string_with_double_slash_counts_as_code`).
pub fn count_source_stats(contents: &str, tokens: &[Token]) -> rock_core::CompileStats {
    let total_lines = contents.lines().count();
    let bytes = contents.len();

    // Per-line classification: 0 = blank, 1 = comment-only, 2 = code.
    // A line is "code" if ANY non-comment / non-newline token touches it
    // (multi-line strings count their full span as code). Comment-only
    // lines come from `TokenKind::Comment`. Everything else is blank.
    let mut line_kind: Vec<u8> = vec![0; total_lines];
    for tok in tokens {
        let val = match &tok.kind {
            TokenKind::Newline => continue,
            TokenKind::Comment(_) => 1u8,
            _ => 2u8,
        };
        let start = tok.span.line_range.0;
        let end = tok.span.line_range.1;
        if total_lines == 0 {
            continue;
        }
        let end = end.min(total_lines - 1);
        let start = start.min(end);
        for slot in &mut line_kind[start..=end] {
            if *slot < val {
                *slot = val;
            }
        }
    }

    let mut stats = rock_core::CompileStats::new();
    stats.files = 1;
    stats.total_lines = total_lines;
    stats.bytes = bytes;
    for k in line_kind {
        match k {
            0 => stats.blank_lines += 1,
            1 => stats.comment_lines += 1,
            _ => stats.code_lines += 1,
        }
    }
    stats
}

pub struct Tokenizer<'a> {
    pub source_file: Ustr,
    pub input: &'a str,
    pub current: Option<char>,

    pub byte_offset: usize,
    pub line: usize,
    pub column: usize,

    // Byte position within `input` (NOT a char index). All ASCII fast-paths
    // use this directly against `input.as_bytes()`. UTF-8 decoding goes
    // through `decode_at` for `current` and `peek_n`.
    byte_index: usize,

    // Track the last token for context-aware parsing
    last_token_kind: Option<TokenKind>,

    // Track if we just skipped whitespace
    just_skipped_whitespace: bool,
}

/// Decode the UTF-8 char starting at byte index `i` in `bytes`, plus its
/// length in bytes. ASCII fast-path (one byte) avoids the `str::from_utf8`
/// machinery for the common case.
#[inline(always)]
fn decode_at(bytes: &[u8], i: usize) -> Option<(char, usize)> {
    let b = *bytes.get(i)?;
    if b < 0x80 {
        return Some((b as char, 1));
    }
    let s = unsafe { std::str::from_utf8_unchecked(&bytes[i..]) };
    let c = s.chars().next()?;
    Some((c, c.len_utf8()))
}

/// Punctuation that is always exactly one character, whatever follows it.
fn single_char_token(ch: char) -> Option<TokenKind> {
    Some(match ch {
        '(' => TokenKind::OpenParen,
        ')' => TokenKind::CloseParen,
        '[' => TokenKind::OpenSquare,
        ']' => TokenKind::CloseSquare,
        '{' => TokenKind::OpenCurly,
        '}' => TokenKind::CloseCurly,
        '%' => TokenKind::Mod,
        ';' => TokenKind::Semicolon,
        ',' => TokenKind::Comma,
        '@' => TokenKind::At,
        '$' => TokenKind::Dollar,
        '^' => TokenKind::BitXor,
        '~' => TokenKind::BitNot,
        _ => return None,
    })
}

impl<'a> Tokenizer<'a> {
    fn read_quoted_text(&mut self, delimiter: char) -> String {
        let mut text = String::new();
        while let Some(next) = self.current {
            self.advance();
            if next == delimiter { break; }
            text.push(next);
        }
        text
    }

    fn at(&self, want: char) -> bool {
        self.current == Some(want)
    }

    fn eat(&mut self, want: char) -> bool {
        if self.at(want) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Create a tokenizer from a SourceFile Arc (convenience method for tests and utils)
    pub fn from_source_file(input: &'a str, source_file: &Arc<AtomicRefCell<SourceFile>>) -> Self {
        let file_path = source_file.borrow().file.to_id_path();
        let file_ustr: Ustr = file_path.into();
        Self::new(input, file_ustr)
    }

    pub fn new(input: &'a str, source_file: Ustr) -> Self {
        let current = decode_at(input.as_bytes(), 0).map(|(c, _)| c);

        Self {
            source_file,
            input,
            current,

            byte_offset: 0,
            line: 0,
            column: 0,
            byte_index: 0,
            last_token_kind: None,
            just_skipped_whitespace: false,
        }
    }

    /// Build a tokenizer that emits spans pointing into a *parent* source.
    /// Used by the interpolated-string and format-specifier parsers to
    /// re-parse a slice of an outer string literal while preserving the
    /// outer file/line/column so diagnostics, hover, and goto-def land in
    /// the right place.
    ///
    /// `offset`, `line`, `column` describe the position of `input[0]`
    /// within the outer source (byte offset, 0-based line, 0-based column).
    /// The three are **independently sourced** — there is no cross-check
    /// at construction. Get them all from the same span and you're fine;
    /// mix-and-match at your peril.
    pub fn new_with_offset(
        input: &'a str,
        source_file: Ustr,
        offset: usize,
        line: usize,
        column: usize,
    ) -> Self {
        let current = decode_at(input.as_bytes(), 0).map(|(c, _)| c);

        Self {
            source_file,
            input,
            current,
            byte_offset: offset,
            line,
            column,
            byte_index: 0,
            last_token_kind: None,
            just_skipped_whitespace: false,
        }
    }

    pub fn from_code(id_path: &str, input: &'a str) -> Self {
        let source_file: Ustr = id_path.into();
        let current = decode_at(input.as_bytes(), 0).map(|(c, _)| c);

        Self {
            source_file,
            input,
            current,
            byte_offset: 0,
            line: 0,
            column: 0,
            byte_index: 0,
            last_token_kind: None,
            just_skipped_whitespace: false,
        }
    }

    pub fn advance(&mut self) {
        // Before advancing, check if current char is a newline
        // If so, the next character starts at column 0
        if self.current == Some('\n') {
            self.line += 1;
            self.column = 0;
        } else if self.current.is_some() {
            // Only increment column if we're not at the start or after a newline
            self.column += 1;
        }

        // Byte offset into the source buffer — matches `Span.offset_range`
        // and what `&str` slicing wants. Increment by the UTF-8 byte
        // length of the consumed character.
        let bytes = self.input.as_bytes();
        if let Some(c) = self.current {
            let len = c.len_utf8();
            self.byte_offset += len;
            self.byte_index += len;
        }

        self.current = decode_at(bytes, self.byte_index).map(|(c, _)| c);
    }

    pub fn peek_n(&self, n: usize) -> Option<char> {
        // `n` is the number of CHARS to skip ahead from the current
        // position. ASCII fast-path: if every byte from byte_index up to
        // (byte_index + n) is ASCII, peeking n chars == peeking n bytes.
        let bytes = self.input.as_bytes();
        let start = self.byte_index;
        if start + n < bytes.len() {
            let mut all_ascii = true;
            for i in 0..n {
                if bytes[start + i] >= 0x80 {
                    all_ascii = false;
                    break;
                }
            }
            if all_ascii {
                let b = bytes[start + n];
                if b < 0x80 {
                    return Some(b as char);
                }
                return decode_at(bytes, start + n).map(|(c, _)| c);
            }
        }
        // Slow path: walk chars from current position.
        self.input[start..].chars().nth(n)
    }

    fn skip_whitespace(&mut self) {
        // Hot path: consume runs of plain space. Operate directly on bytes
        // (ASCII) — no UTF-8 decode needed for ' '. Updates column/offset
        // bookkeeping in lockstep with `advance` for a known-ASCII char.
        let bytes = self.input.as_bytes();
        let mut i = self.byte_index;
        let n = bytes.len();
        let mut consumed = 0usize;
        while i < n && bytes[i] == b' ' {
            i += 1;
            consumed += 1;
        }
        if consumed > 0 {
            self.byte_index = i;
            self.byte_offset += consumed;
            self.column += consumed;
            self.just_skipped_whitespace = true;
            self.current = decode_at(bytes, i).map(|(c, _)| c);
        }
    }

    /// Scan an identifier and return it as a borrowed slice of `input` -- no
    /// per-char `String`, no per-char `advance`. The cursor and line/column
    /// bookkeeping are advanced past the whole identifier in one step.
    ///
    /// The returned slice has the input's lifetime `'a`, not the `&mut self`
    /// borrow, so the caller can match it against keywords and intern it
    /// without copying. `next_token` only enters here on an ASCII alpha / `_`,
    /// so the first byte is ASCII; the tight loop stays on bytes and only
    /// drops to UTF-8 decoding when a non-ASCII byte appears (Unicode letters
    /// mid-identifier), preserving the old `char::is_alphanumeric` behavior.
    fn read_ident(&mut self) -> &'a str {
        let input = self.input; // copy the &'a str out, decoupled from &mut self
        let bytes = input.as_bytes();
        let n = bytes.len();
        let start = self.byte_index;
        let mut i = start;
        let mut char_count = 0usize;

        while i < n {
            let b = bytes[i];
            if b.is_ascii_alphanumeric() || b == b'_' {
                i += 1;
                char_count += 1;
            } else if b < 0x80 {
                break;
            } else {
                match decode_at(bytes, i) {
                    Some((c, len)) if c.is_alphanumeric() => {
                        i += len;
                        char_count += 1;
                    }
                    _ => break,
                }
            }
        }

        // One bookkeeping update for the whole identifier. Identifiers contain
        // no newlines, so `line` is unchanged; `column` counts chars (not
        // bytes) to match `advance`, and `byte_offset` counts bytes.
        self.byte_index = i;
        self.byte_offset += i - start;
        self.column += char_count;
        self.current = decode_at(bytes, i).map(|(c, _)| c);
        &input[start..i]
    }

    /// Scan from the current position to the end of the line (NOT consuming the
    /// terminating `\n`) and return the borrowed slice -- for line comments,
    /// whose body is verbatim with no escapes. The body holds no newline, so
    /// `line` is unchanged; `column` advances by the char count (counted from
    /// UTF-8 lead bytes, i.e. bytes that are not `10xxxxxx` continuations).
    fn read_line_rest(&mut self) -> &'a str {
        let input = self.input;
        let bytes = input.as_bytes();
        let n = bytes.len();
        let start = self.byte_index;
        let mut i = start;
        let mut char_count = 0usize;
        while i < n && bytes[i] != b'\n' {
            if bytes[i] & 0xC0 != 0x80 {
                char_count += 1;
            }
            i += 1;
        }
        self.byte_index = i;
        self.byte_offset += i - start;
        self.column += char_count;
        self.current = decode_at(bytes, i).map(|(c, _)| c);
        &input[start..i]
    }

    /// Scan a `"""`-delimited multiline string body from the current position
    /// (just past the opening `"""`) up to -- but NOT consuming -- the closing
    /// `"""`. The body is verbatim (no escapes), so it's returned as a borrowed
    /// slice; the cursor plus full line/column bookkeeping (newlines included)
    /// are advanced over the body. Returns `None` on EOF before a closing
    /// `"""`, having advanced to EOF so the caller's error span is sane.
    fn read_triple_quote_body(&mut self) -> Option<&'a str> {
        let input = self.input;
        let bytes = input.as_bytes();
        let n = bytes.len();
        let start = self.byte_index;
        let mut i = start;
        let mut line = self.line;
        let mut col = self.column;
        let found = loop {
            if i + 2 < n && bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                break true;
            }
            if i >= n {
                break false;
            }
            if bytes[i] == b'\n' {
                line += 1;
                col = 0;
                i += 1;
            } else {
                if bytes[i] & 0xC0 != 0x80 {
                    col += 1;
                }
                i += 1;
            }
        };
        self.byte_index = i;
        self.byte_offset += i - start;
        self.line = line;
        self.column = col;
        self.current = decode_at(bytes, i).map(|(c, _)| c);
        if found {
            Some(&input[start..i])
        } else {
            None
        }
    }

    /// Fast path for a single-line string with no escapes, interpolation, or
    /// raw newline: scan to the closing `"` and return a verbatim slice. Bails
    /// (returns `None`, cursor untouched) the instant a `\`, `{`, or newline
    /// appears, so the caller falls back to [`read_escaped_string`]. The
    /// opening `"` must already be consumed. `{` forces the slow path because
    /// interpolation makes a later `"` non-closing; `}` and `}}` are verbatim
    /// and stay on the fast path, matching the slow path's behavior.
    fn try_read_plain_string(&mut self) -> Option<&'a str> {
        let input = self.input;
        let bytes = input.as_bytes();
        let n = bytes.len();
        let start = self.byte_index;
        let mut j = start;
        let mut char_count = 0usize;
        loop {
            if j >= n {
                return None; // unterminated -- let the slow path raise the error
            }
            match bytes[j] {
                b'"' => {
                    // consume the body and the closing quote
                    self.byte_index = j + 1;
                    self.byte_offset += (j + 1) - start;
                    self.column += char_count + 1; // +1 for the closing quote
                    self.current = decode_at(bytes, j + 1).map(|(c, _)| c);
                    return Some(&input[start..j]);
                }
                b'\\' | b'{' | b'\n' => return None,
                b => {
                    if b & 0xC0 != 0x80 {
                        char_count += 1;
                    }
                    j += 1;
                }
            }
        }
    }

    /// Slow path for single-line strings: processes escape sequences (`\n`,
    /// `\t`, ...) and tracks `{}` interpolation depth, including nested string
    /// literals inside `{...}`. The opening `"` must already be consumed. Used
    /// when [`try_read_plain_string`] bails.
    fn read_escaped_string(&mut self) -> Result<TokenKind, CompilerError> {
        let mut string = String::new();
        let mut interp_depth: usize = 0; // Track {}-nesting for interpolation
        // `closed` distinguishes clean termination on the
        // matching `"` from falling out of the loop because
        // we ran off the end of input. Without this flag,
        // `"foo{"` (bare `{` with no closing `}`) silently
        // compiled to a garbage string because the nested-
        // string handler gobbled the rest of the file
        // looking for a closing quote.
        let mut closed = false;
        let string_open_span = self.start_span();
        while let Some(ch) = self.current {
            if ch == '"' && interp_depth == 0 {
                // Closing quote — only if not inside an interpolation expression
                self.advance();
                closed = true;
                break;
            }
            if ch == '"' && interp_depth > 0 {
                // String literal inside interpolation: skip to its closing quote
                string.push(ch);
                self.advance();
                let mut inner_closed = false;
                while let Some(inner_ch) = self.current {
                    string.push(inner_ch);
                    self.advance();
                    if inner_ch == '\\' {
                        // Escape inside nested string
                        if let Some(esc) = self.current {
                            string.push(esc);
                            self.advance();
                        }
                    } else if inner_ch == '"' {
                        inner_closed = true;
                        break;
                    }
                }
                if !inner_closed {
                    // Most likely the user wrote a bare `{`
                    // that was meant to be literal; the lexer
                    // treated it as opening an interpolation,
                    // then hit a nested `"` and gobbled the
                    // rest of the input looking for its
                    // close. Surface the probable root cause
                    // in the error message.
                    return Err(CompilerError::new_with_label(
                        self.end_span(string_open_span),
                        "unterminated string literal: a bare `{` opens an interpolation; use `{{` to emit a literal `{`".to_string(),
                    ));
                }
                continue;
            }
            // Doubled-brace escapes (`{{` / `}}`) are only
            // literal when we're NOT inside an interpolation
            // expression (depth == 0). Inside `{...}` the
            // braces are part of the expression's own syntax.
            // We consume both chars as a single literal and
            // DON'T touch interp_depth, so the lexer keeps
            // looking for the real string terminator.
            if ch == '{' && interp_depth == 0 && self.peek_n(1) == Some('{') {
                string.push('{');
                string.push('{');
                self.advance();
                self.advance();
                continue;
            }
            if ch == '}' && interp_depth == 0 && self.peek_n(1) == Some('}') {
                string.push('}');
                string.push('}');
                self.advance();
                self.advance();
                continue;
            }

            if ch == '{' {
                interp_depth += 1;
            } else if ch == '}' && interp_depth > 0 {
                interp_depth -= 1;
            }
            if ch == '\\' {
                self.advance();
                if let Some(escaped_ch) = self.current {
                    match escaped_ch {
                        'n' => string.push('\n'),
                        't' => string.push('\t'),
                        '\\' => string.push('\\'),
                        '"' => string.push('"'),
                        '\'' => string.push('\''),
                        other => string.push(other),
                    }
                    self.advance();
                } else {
                    return Err(CompilerError::new_with_label(
                        self.end_span(self.start_span()),
                        "Unexpected escape at end of input".to_string(),
                    ));
                }
            } else {
                string.push(ch);
                self.advance();
            }
        }

        // Post-loop checks for diagnostic failure modes.
        //
        // - `!closed`: we ran off the end of input without ever
        //   seeing the matching `"`. Either the string literal
        //   is simply unterminated, OR a bare `{` started an
        //   interpolation that was never closed — in which
        //   case the nested-string handler above would have
        //   consumed past the intended terminator.
        // - `interp_depth > 0`: more specific — we saw the
        //   closing `"` but there's still an open `{` on the
        //   interpolation stack. (Unreachable in practice
        //   because `"` at depth>0 goes through the nested
        //   handler, but kept as a belt-and-braces check.)
        //
        // The actionable hint steers users toward `{{` for
        // literal open-braces, which is the only way in
        // Rock to put a literal `{` inside a string.
        if !closed {
            return Err(CompilerError::new_with_label(
                self.end_span(string_open_span),
                "unterminated string literal: a bare `{` opens an interpolation that must be closed with `}`; use `{{` to emit a literal `{`".to_string(),
            ));
        }
        if interp_depth > 0 {
            return Err(CompilerError::new_with_label(
                self.end_span(string_open_span),
                "unmatched `{` in string literal: use `{{` to emit a literal `{`".to_string(),
            ));
        }

        Ok(TokenKind::String(string.into()))
    }

    fn read_keyword_or_ident(&mut self) -> Result<TokenKind, CompilerError> {
        // Check if this might be a raw string literal (r" or r""")
        if self.current == Some('r') && (self.peek_n(1) == Some('"')) {
            // Save current position in case we need to backtrack
            let start_byte_index = self.byte_index;
            let start_byte_offset = self.byte_offset;
            let start_line = self.line;
            let start_column = self.column;

            self.advance(); // consume 'r'

            // Check for multiline raw string r"""
            if self.current == Some('"')
                && self.peek_n(1) == Some('"')
                && self.peek_n(2) == Some('"')
            {
                self.advance(); // consume first "
                self.advance(); // consume second "
                self.advance(); // consume third "

                let mut string = String::new();
                let mut quote_count = 0;

                while let Some(ch) = self.current {
                    if ch == '"' {
                        quote_count += 1;
                        self.advance();
                        if quote_count == 3 {
                            break;
                        }
                    } else {
                        // If we had some quotes but not 3, add them to the string
                        for _ in 0..quote_count {
                            string.push('"');
                        }
                        quote_count = 0;
                        string.push(ch);
                        self.advance();
                    }
                }

                if quote_count != 3 {
                    return Err(CompilerError::new_with_label(
                        self.end_span(self.start_span()),
                        "unterminated multiline raw string".to_string(),
                    ));
                }

                return Ok(TokenKind::RawString(string.into()));
            }
            // Check for single-line raw string r"
            else if self.current == Some('"') {
                self.advance(); // consume opening "

                let mut text = String::new();
                while let Some(ch) = self.current {
                    if ch == '"' {
                        self.advance(); // consume closing "
                        return Ok(TokenKind::RawString(text.into()));
                    }
                    text.push(ch);
                    self.advance();
                }

                return Err(CompilerError::new_with_label(
                    self.end_span(self.start_span()),
                    "unterminated raw string".to_string(),
                ));
            }
            // Not a raw string, backtrack and continue with normal identifier parsing
            else {
                self.byte_index = start_byte_index;
                self.byte_offset = start_byte_offset;
                self.line = start_line;
                self.column = start_column;
                self.current = decode_at(self.input.as_bytes(), self.byte_index).map(|(c, _)| c);
            }
        }

        let ident = self.read_ident();

        match ident {
            "fn" => Ok(TokenKind::Fn),
            "operator" => Ok(TokenKind::Operator),
            "table" => Ok(TokenKind::Table),
            "type" => Ok(TokenKind::Type),
            "as" => Ok(TokenKind::As),

            "yield" => Ok(TokenKind::Yield),

            "if" => Ok(TokenKind::If),

            "let" => Ok(TokenKind::Let),
            "return" => Ok(TokenKind::Return),
            "defer" => Ok(TokenKind::Defer),

            "struct" => Ok(TokenKind::Struct),
            "newtype" => Ok(TokenKind::Newtype),

            "else" => Ok(TokenKind::Else),
            "for" => Ok(TokenKind::For),
            "in" => Ok(TokenKind::In),
            "while" => Ok(TokenKind::While),
            "loop" => Ok(TokenKind::Loop),
            "break" => Ok(TokenKind::Break),
            "continue" => Ok(TokenKind::Continue),
            "null" => Ok(TokenKind::Null),
            "enum" => Ok(TokenKind::Enum),
            "match" => Ok(TokenKind::Match),
            "extern" => Ok(TokenKind::Extern),
            "global" => Ok(TokenKind::Global),
            "const" => Ok(TokenKind::Const),

            "deco" => Ok(TokenKind::Deco),
            // Note: "wrap" and "init" are NOT keywords - they are contextual
            // and only recognized in the context of decorator definitions

            "macro" => Ok(TokenKind::Macro),

            "true" => Ok(TokenKind::True),
            "false" => Ok(TokenKind::False),

            "mut" => Err(CompilerError::new_with_label(
                self.empty_span(),
                "`let mut` is not supported, all bindings are mut, use just `let`".to_string(),
            )),

            _ => {
                // Escape Lua keywords that are valid Rock identifiers
                // This prevents conflicts when generating Lua code
                let escaped_ident: &str = match ident {
                    // Lua keywords that are NOT Rock keywords
                    "and" => "__rock_and",
                    "do" => "__rock_do",
                    "elseif" => "__rock_elseif",
                    "end" => "__rock_end",
                    "function" => "__rock_function",
                    "goto" => "__rock_goto",
                    "local" => "__rock_local",
                    "nil" => "__rock_nil",
                    "not" => "__rock_not",
                    "or" => "__rock_or",
                    "repeat" => "__rock_repeat",
                    "then" => "__rock_then",
                    "until" => "__rock_until",
                    _ => ident,
                };
                Ok(TokenKind::Ident(escaped_ident.into()))
            }
        }
    }

    /// Convert an escaped Lua keyword back to its original form
    /// This is the inverse of the escaping done during tokenization
    pub fn unescape_lua_keyword(name: &str) -> &str {
        match name {
            "__rock_and" => "and",
            "__rock_do" => "do",
            "__rock_elseif" => "elseif",
            "__rock_end" => "end",
            "__rock_function" => "function",
            "__rock_goto" => "goto",
            "__rock_local" => "local",
            "__rock_nil" => "nil",
            "__rock_not" => "not",
            "__rock_or" => "or",
            "__rock_repeat" => "repeat",
            "__rock_then" => "then",
            "__rock_until" => "until",
            _ => name,
        }
    }

    pub fn lookahead_number(&self) -> bool {
        match self.current {
            Some('0'..='9') => true,
            Some('-') => {
                let numeric = self.peek_n(1).is_some_and(|c| c.is_ascii_digit()) || (self.peek_n(1) == Some('.') && self.peek_n(2).is_some_and(|c| c.is_ascii_digit()));
                numeric && self.minus_can_start_number()
            },
            _ => false,
        }
    }

    /// Determines if a '-' can start a negative number based on the previous token context
    fn minus_can_start_number(&self) -> bool {
        // If we just skipped whitespace, '-' can start a negative number
        if self.just_skipped_whitespace {
            return true;
        }

        match &self.last_token_kind {
            // '-' can start a negative number after these tokens
            None => true, // Start of input
            Some(TokenKind::OpenParen) => true,
            Some(TokenKind::OpenSquare) => true,
            Some(TokenKind::OpenCurly) => true,
            Some(TokenKind::Comma) => true,
            Some(TokenKind::Semicolon) => true,
            Some(TokenKind::Colon) => true,
            Some(TokenKind::Assign) => true,
            Some(TokenKind::AddAssign) => true,
            Some(TokenKind::SubAssign) => true,
            Some(TokenKind::MulAssign) => true,
            Some(TokenKind::DivAssign) => true,
            Some(TokenKind::Equals) => true,
            Some(TokenKind::NotEquals) => true,
            Some(TokenKind::LessThan) => true,
            Some(TokenKind::LessThanEqual) => true,
            Some(TokenKind::GreaterThan) => true,
            Some(TokenKind::GreaterThanEqual) => true,
            Some(TokenKind::And) => true,
            Some(TokenKind::Or) => true,
            Some(TokenKind::Plus) => true,
            Some(TokenKind::Minus) => true,
            Some(TokenKind::Mul) => true,
            Some(TokenKind::Div) => true,
            Some(TokenKind::Mod) => true,
            Some(TokenKind::Bang) => true,
            Some(TokenKind::DoubleDot) => true, // Range operator
            Some(TokenKind::Arrow) => true,
            Some(TokenKind::FatArrow) => true,
            Some(TokenKind::If) => true,
            Some(TokenKind::Else) => true,
            Some(TokenKind::While) => true,
            Some(TokenKind::For) => true,
            Some(TokenKind::In) => true,
            Some(TokenKind::Return) => true,
            Some(TokenKind::Let) => true,
            Some(TokenKind::Newline) => true,

            // '-' should be a binary operator after these tokens (identifiers, literals, closing brackets)
            // ONLY if there was no whitespace between them
            Some(TokenKind::Ident(_)) => false,
            Some(TokenKind::Int(_, _)) => false,
            Some(TokenKind::Float(_)) => false,
            Some(TokenKind::String(_)) => false,
            Some(TokenKind::RawString(_)) => false,
            Some(TokenKind::True) => false,
            Some(TokenKind::False) => false,
            Some(TokenKind::Null) => false,
            Some(TokenKind::CloseParen) => false,
            Some(TokenKind::CloseSquare) => false,
            Some(TokenKind::CloseCurly) => false,

            // For any other tokens, default to allowing negative numbers (conservative approach)
            _ => true,
        }
    }

    pub fn read_number(&mut self) -> Result<TokenKind, CompilerError> {
        let origin = self.start_span();
        let mut digits = String::new();
        let mut spelling = String::new();
        let mut radix = 10u32;
        let mut floaty = false;
        let mut dotted = false;
        if self.current == Some('-') {
            digits.push('-');
            spelling.push('-');
            self.advance();
        }
        if self.current == Some('0') {
            radix = match self.peek_n(1) {
                Some('b' | 'B') => 2,
                Some('x' | 'X') => 16,
                _ => 10,
            };
            if radix != 10 {
                spelling.push_str(if radix == 2 { "0b" } else { "0x" });
                self.advance();
                self.advance();
            } else {
                digits.push('0');
                spelling.push('0');
                self.advance();
            }
        }
        loop {
            let Some(next) = self.current else { break };
            if radix != 10 {
                let valid = match radix {
                    2 => matches!(next, '0' | '1'),
                    _ => next.is_ascii_hexdigit(),
                };
                if valid {
                    digits.push(next);
                } else if next != '_' {
                    break;
                }
                spelling.push(next);
                self.advance();
                continue;
            }
            match next {
                '0'..='9' => {},
                '.' => {
                    if dotted || self.peek_n(1) == Some('.') || self.peek_n(1).is_some_and(|c| c.is_alphabetic() || c == '_') || matches!(&self.last_token_kind, Some(TokenKind::Dot)) { break; }
                    dotted = true;
                    floaty = true;
                },
                'e' | 'E' => {
                    floaty = true;
                    digits.push(next);
                    spelling.push(next);
                    self.advance();
                    if let Some(sign @ ('+' | '-')) = self.current {
                        digits.push(sign);
                        spelling.push(sign);
                        self.advance();
                    }
                    let mut exponent_digits = 0usize;
                    while self.current.is_some_and(|c| c.is_ascii_digit()) {
                        let digit = self.current.unwrap();
                        digits.push(digit);
                        spelling.push(digit);
                        exponent_digits += 1;
                        self.advance();
                    }
                    if exponent_digits == 0 {
                        return Err(CompilerError::new_with_label(
                            self.end_span(origin.clone()),
                            format!("Invalid scientific notation: expected digits after exponent in '{}'", spelling),
                        ).with_code(CompilerErrorCode::TokenizerError));
                    }
                    break;
                },
                _ => break,
            }
            digits.push(next);
            spelling.push(next);
            self.advance();
        }
        if radix != 10 {
            let label = if radix == 2 { "binary" } else { "hex" };
            let prefix = if radix == 2 { "0b" } else { "0x" };
            if digits.is_empty() {
                return Err(CompilerError::new_with_label(
                    self.end_span(origin.clone()),
                    format!("Invalid {label} literal: no digits after {prefix}"),
                ).with_code(CompilerErrorCode::TokenizerError));
            }
            let value = i64::from_str_radix(&digits, radix).map_err(|error| {
                CompilerError::new_with_label(
                    self.end_span(origin.clone()),
                    format!("Invalid {label} literal '{}': {}", digits, error),
                ).with_code(CompilerErrorCode::TokenizerError)
            })?;
            return Ok(TokenKind::Int(value, spelling.into()));
        }
        let token = if floaty {
            let value = digits.parse().map_err(|_| {
                CompilerError::new_with_label(self.end_span(origin.clone()),
                    format!("Invalid floating point literal: '{}'", digits))
                    .with_code(CompilerErrorCode::TokenizerError)
            })?;
            TokenKind::Float(value)
        } else {
            let value = digits.parse::<i64>().map_err(|error| {
                CompilerError::new_with_label(self.end_span(origin.clone()),
                    format!("Invalid integer literal '{}': {}", digits, error))
                    .with_code(CompilerErrorCode::TokenizerError)
            })?;
            TokenKind::Int(value, spelling.into())
        };
        Ok(token)
    }

    pub fn next_token(&mut self) -> Result<Option<Token>, CompilerError> {
        self.skip_whitespace();

        let start = self.start_span();

        let kind = match self.current {
            None => return Ok(None),
            Some(ch) if ch.is_ascii_alphabetic() || ch == '_' => self.read_keyword_or_ident()?,

            // `lookahead_number` only ever returns true for a digit or `-`
            // (`.` and everything else are false), so gate the call to those
            // two chars instead of paying it on every operator/punctuation.
            Some(ch) if ch.is_ascii_digit() => self.read_number()?,
            Some('-') if self.lookahead_number() => self.read_number()?,

            Some('\r') => {
                self.advance();
                if self.current == Some('\n') {
                    self.advance();

                    TokenKind::Newline
                } else {
                    return Err(CompilerError::new_with_label(
                        Span::unknown(),
                        format!(
                            "Broken newlines on windows? Expecting \r\n, got: {:?}",
                            self.current
                        ),
                    ));
                }
            }
            Some('\n') => {
                self.advance();

                TokenKind::Newline
            }

            Some('-') => {
                self.advance();
                if self.eat('>') {
                    TokenKind::Arrow
                } else if self.eat('=') {
                    TokenKind::SubAssign
                } else if self.eat('-') {
                    TokenKind::MinusMinus
                } else {
                    TokenKind::Minus
                }
            }

            Some('+') => {
                self.advance();
                if self.eat('+') {
                    TokenKind::PlusPlus
                } else if self.eat('=') {
                    TokenKind::AddAssign
                } else {
                    TokenKind::Plus
                }
            }

            Some('*') => {
                self.advance();
                if self.eat('=') {
                    TokenKind::MulAssign
                } else {
                    TokenKind::Mul
                }
            }

            Some('/') => {
                self.advance();
                if self.eat('=') {
                    TokenKind::DivAssign
                } else if self.eat('/') {
                    TokenKind::Comment(self.read_line_rest().into())
                } else if self.eat('*') {
                    let mut body = String::with_capacity(16);
                    loop {
                        let Some(cur) = self.current else { break };
                        if cur == '*' {
                            self.advance();
                            if self.eat('/') {
                                break;
                            }
                            body.push('*');
                        } else {
                            body.push(cur);
                            self.advance();
                        }
                    }
                    TokenKind::Comment(body.into())
                } else {
                    TokenKind::Div
                }
            }

            Some('=') => {
                self.advance();
                if self.eat('=') {
                    TokenKind::Equals
                } else if self.eat('>') {
                    TokenKind::FatArrow
                } else {
                    TokenKind::Assign
                }
            }

            Some('!') => {
                self.advance();
                if self.eat('=') {
                    TokenKind::NotEquals
                } else {
                    TokenKind::Bang
                }
            }

            Some('<') => {
                self.advance();
                if self.eat('=') {
                    TokenKind::LessThanEqual
                } else if self.eat('<') {
                    TokenKind::LeftShift
                } else {
                    TokenKind::LessThan
                }
            }

            Some('>') => {
                self.advance();
                if self.eat('=') {
                    TokenKind::GreaterThanEqual
                } else if self.eat('>') {
                    TokenKind::RightShift
                } else {
                    TokenKind::GreaterThan
                }
            }

            Some(':') => {
                self.advance();
                if self.eat(':') {
                    TokenKind::DoubleColon
                } else {
                    TokenKind::Colon
                }
            }

            Some('.') => {
                let mut width = 0;
                while width < 3 && self.current == Some('.') {
                    self.advance();
                    width += 1;
                }
                match width {
                    1 => TokenKind::Dot,
                    2 => TokenKind::DoubleDot,
                    _ => TokenKind::TripleDot,
                }
            }

            Some('?') => {
                self.advance();
                if self.eat('?') {
                    if self.eat('?') {
                        TokenKind::TripleQuestionMark
                    } else {
                        TokenKind::DoubleQuestionMark
                    }
                } else if self.eat('.') {
                    TokenKind::QuestionDot
                } else {
                    TokenKind::QuestionMark
                }
            }

            Some('"') => {
                // detect triple-quote multiline string """ ... """
                if self.peek_n(1) == Some('"') && self.peek_n(2) == Some('"') {
                    // consume the opening """
                    self.advance();
                    self.advance();
                    self.advance();
                    match self.read_triple_quote_body() {
                        Some(body) => {
                            // consume the closing """
                            self.advance();
                            self.advance();
                            self.advance();
                            TokenKind::String(body.into())
                        }
                        None => {
                            return Err(CompilerError::new_with_label(
                                self.end_span(self.start_span()),
                                "unterminated multiline string".to_string(),
                            ));
                        }
                    }
                } else {
                    // single-line string
                    self.advance(); // consume opening "
                    // Fast path: plain string with no escape / interpolation /
                    // raw newline -> a verbatim slice. Anything else falls back
                    // to the full escape + interpolation scanner.
                    if let Some(body) = self.try_read_plain_string() {
                        TokenKind::String(body.into())
                    } else {
                        self.read_escaped_string()?
                    }
                }
            }

            Some('&') => {
                self.advance();
                if self.eat('&') {
                    TokenKind::And
                } else {
                    TokenKind::BitAnd
                }
            }

            Some('|') => {
                self.advance();
                if self.eat('|') {
                    TokenKind::Or
                } else {
                    TokenKind::BitOr
                }
            }

            Some('\'') => {
                self.advance();
                let text = self.read_quoted_text('\'');
                TokenKind::String(text.into())
            }

            Some('#') => {
                // Peek ahead to determine what kind of # construct this is
                if let Some(next_ch) = self.peek_n(1) {
                    // Check if it's a lowercase letter (potential directive)
                    if next_ch.is_ascii_lowercase() {
                        self.advance(); // consume #
                        let input = self.input;
                        let word_start = self.byte_index;

                        // Read the directive word (ASCII alphabetic only) as a
                        // byte slice -- no per-char String.
                        {
                            let bytes = input.as_bytes();
                            let mut i = self.byte_index;
                            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                                i += 1;
                            }
                            let consumed = i - self.byte_index;
                            self.byte_index = i;
                            self.byte_offset += consumed;
                            self.column += consumed; // all ASCII: chars == bytes
                            self.current = decode_at(bytes, i).map(|(c, _)| c);
                        }

                        // Check if it's a known directive
                        match &input[word_start..self.byte_index] {
                            "if" => TokenKind::DirectiveIf,
                            "elseif" => TokenKind::DirectiveElseIf,
                            "else" => TokenKind::DirectiveElse,
                            "end" => TokenKind::DirectiveEnd,
                            _ => {
                                // Not a directive: the comment is the word plus
                                // the rest of the line -- contiguous in source,
                                // so re-slice from the word start to the newline.
                                let _ = self.read_line_rest();
                                TokenKind::Comment(input[word_start..self.byte_index].into())
                            }
                        }
                    } else {
                        // Regular # comment
                        self.advance(); // consume #
                        TokenKind::Comment(self.read_line_rest().into())
                    }
                } else {
                    // Just # at end of file: treat as an empty comment.
                    self.advance();
                    TokenKind::Comment(String::new().into())
                }
            }

            Some(ch) => match single_char_token(ch) {
                Some(kind) => {
                    self.advance();
                    kind
                }
                None => {
                    return Err(CompilerError::new_with_label(
                        Span::unknown(),
                        format!("I done a bad thing: {:?}", self.current),
                    ))
                }
            },
        };

        // return Err(CompilerError::new(self.start_span(), "fart".to_string()));
        //
        // let span = self.end_span(start.clone());
        //
        // self.end_span(start.clone()).print(format!("line {:?} col {:?} offset {:?}", span.line_range, span.col_range, span.offset_range));
        //
        // println!(" ... end -> {:?}", self.end_span(start.clone()));

        // Update last token kind for context-aware parsing. Skip newlines
        // and comments — both are non-meaningful trivia for tokenization
        // decisions like "is the next `.` a tuple index or a decimal?"
        if !matches!(kind, TokenKind::Newline | TokenKind::Comment(_)) {
            self.last_token_kind = Some(kind.clone());
        }

        // Reset the whitespace flag for next token
        self.just_skipped_whitespace = false;

        Ok(Some(Token {
            kind,
            span: self.end_span(start),
            trailing_newlines: 0,
        }))
    }

    pub fn start_span(&self) -> LineCol {
        LineCol {
            file: self.source_file,
            line: self.line,
            col: self.column,
            offset: self.byte_offset,
        }
    }

    pub fn end_span(&self, start: LineCol) -> Span {
        // `col_range` end is exclusive. Use `self.column` directly — it
        // tracks columns in characters (incremented per char by `advance`),
        // whereas `byte_offset` counts bytes (UTF-8 length per char), so
        // `start.col + (byte_offset - start.offset)` would skew columns
        // past any multibyte char in a token.
        let end_col = self.column;
        let offset_range = (start.offset, self.byte_offset);

        assert!(
            offset_range.0 <= offset_range.1,
            "Offset range is invalid: {:?}",
            offset_range
        );

        Span {
            file: start.file,
            line_range: (start.line, self.line),
            col_range: (start.col, end_col),
            offset_range,
        }
    }

    pub fn empty_span(&self) -> Span {
        Span {
            file: self.source_file,
            line_range: (self.line, self.line),
            col_range: (self.column, self.column),
            offset_range: (self.byte_offset, self.byte_offset),
        }
    }

    pub fn tokenize(&mut self) -> Result<Vec<Token>, CompilerError> {
        // Tokens average roughly 4-5 source bytes each (idents, operators,
        // whitespace); pre-size to skip the growth reallocations of the output
        // vector. Over-estimates are harmless (one transient allocation).
        let mut tokens: Vec<Token> = Vec::with_capacity(self.input.len() / 4);

        while let Some(token) = self.next_token()? {
            if token.kind == TokenKind::Newline {
                if let Some(last) = tokens.last_mut() {
                    last.trailing_newlines += 1;
                }
            } else {
                tokens.push(token)
            }
        }

        Ok(tokens)
    }

    pub fn tokenize_kinds(&mut self) -> Result<Vec<TokenKind>, CompilerError> {
        self.tokenize()
            .map(|v| v.into_iter().map(|x| x.kind).collect_vec())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_number_tokenization() {
        let tokens = Tokenizer::from_code("test.rock", "1 23 456. 0.4")
            .tokenize_kinds()
            .unwrap();

        assert_eq!(
            tokens,
            vec![
                TokenKind::Int(1, "1".into()),
                TokenKind::Int(23, "23".into()),
                TokenKind::Float(OrderedFloat(456.)),
                TokenKind::Float(OrderedFloat(0.4)),
            ]
        );
    }

    #[test]
    pub fn test_whitespace_skip() {
        let tokens = Tokenizer::from_code("test.rock", "  fn  if \nmatch     while")
            .tokenize_kinds()
            .unwrap();

        assert_eq!(
            tokens,
            vec![
                TokenKind::Fn,
                TokenKind::If,
                TokenKind::Match,
                TokenKind::While
            ]
        );
    }

    #[test]
    pub fn test_trailing_newlines() {
        let tokens = Tokenizer::from_code("test.rock", "fn\n\nfoo")
            .tokenize()
            .unwrap();

        assert_eq!(tokens[0].kind, TokenKind::Fn);
        assert_eq!(tokens[0].trailing_newlines, 2);
        assert_eq!(tokens[1].kind, TokenKind::Ident("foo".into()));
    }

    #[test]
    pub fn test_idents() {
        let tokens = Tokenizer::from_code("test.rock", "hello foo_bar _baz a.b f(x)")
            .tokenize_kinds()
            .unwrap();

        assert_eq!(
            tokens,
            vec![
                TokenKind::Ident("hello".into()),
                TokenKind::Ident("foo_bar".into()),
                TokenKind::Ident("_baz".into()),
                TokenKind::Ident("a".into()),
                TokenKind::Dot,
                TokenKind::Ident("b".into()),
                TokenKind::Ident("f".into()),
                TokenKind::OpenParen,
                TokenKind::Ident("x".into()),
                TokenKind::CloseParen,
            ]
        );
    }

    #[test]
    pub fn test_unicode_ident_byte_vs_char_columns() {
        // Locks the non-ASCII fallback in `read_ident`: an identifier with a
        // multibyte continuation char (`é` = 2 UTF-8 bytes) must still be one
        // ident, with `offset_range` counting BYTES and `col_range` counting
        // CHARS. "café" = 5 bytes / 4 chars; the following `x` starts at byte
        // 6, column 5.
        let tokens = Tokenizer::from_code("test.rock", "café x")
            .tokenize()
            .unwrap();

        assert_eq!(tokens[0].kind, TokenKind::Ident("café".into()));
        assert_eq!(tokens[0].span.offset_range, (0, 5));
        assert_eq!(tokens[0].span.col_range, (0, 4));
        assert_eq!(tokens[1].kind, TokenKind::Ident("x".into()));
        assert_eq!(tokens[1].span.offset_range, (6, 7));
        assert_eq!(tokens[1].span.col_range, (5, 6));
    }

    #[test]
    pub fn test_multiline_string_content_and_line_tracking() {
        // `"""` body is verbatim (incl. the embedded newline), and the newline
        // inside the body MUST advance the line counter, so the trailing `c`
        // lands on line 2 (line0 `"""a`, line1 `b"""`, line2 `c`). A missed
        // newline in the body scan would put `c` on line 1.
        let tokens = Tokenizer::from_code("test.rock", "\"\"\"a\nb\"\"\"\nc")
            .tokenize()
            .unwrap();
        assert_eq!(tokens[0].kind, TokenKind::String("a\nb".into()));
        assert_eq!(tokens[1].kind, TokenKind::Ident("c".into()));
        assert_eq!(tokens[1].span.line_range.0, 2);
    }

    #[test]
    pub fn test_string_fast_and_slow_paths() {
        // Plain string -> fast path (verbatim slice).
        assert_eq!(
            Tokenizer::from_code("t.rock", "\"hello world\"").tokenize_kinds().unwrap(),
            vec![TokenKind::String("hello world".into())]
        );
        // Escapes -> slow path: `\n`/`\t` are transformed, so the fast path
        // MUST bail (a slice would keep the literal backslash).
        assert_eq!(
            Tokenizer::from_code("t.rock", "\"a\\nb\\tc\"").tokenize_kinds().unwrap(),
            vec![TokenKind::String("a\nb\tc".into())]
        );
        // Interpolation braces are kept verbatim (re-parsed later); `{` forces
        // the slow path but the stored text matches a slice anyway.
        assert_eq!(
            Tokenizer::from_code("t.rock", "\"hp={x}\"").tokenize_kinds().unwrap(),
            vec![TokenKind::String("hp={x}".into())]
        );
        // A `"` inside `{...}` must NOT end the outer string -- the fast path's
        // first-quote-wins scan would get this wrong, so `{` bailing is load-bearing.
        assert_eq!(
            Tokenizer::from_code("t.rock", "\"a{f(\"x\")}b\"").tokenize_kinds().unwrap(),
            vec![TokenKind::String("a{f(\"x\")}b".into())]
        );
    }

    #[test]
    pub fn test_numbers() {
        let tokens = Tokenizer::from_code("test.rock", "-1 2.3 -7. -7 -.3")
            .tokenize_kinds()
            .unwrap();

        assert_eq!(
            tokens,
            vec![
                TokenKind::Int(-1, "-1".into()),
                TokenKind::Float(OrderedFloat(2.3)),
                TokenKind::Float(OrderedFloat(-7.0)),
                TokenKind::Int(-7, "-7".into()),
                TokenKind::Float(OrderedFloat(-0.3)),
            ]
        );
    }

    #[test]
    pub fn test_scientific_notation() {
        let tokens = Tokenizer::from_code("test.rock", "1.5e3 2.0E-4 3e10 -1.5e-3 4E+5 5e0")
            .tokenize_kinds()
            .unwrap();

        assert_eq!(
            tokens,
            vec![
                TokenKind::Float(OrderedFloat(1500.0)),   // 1.5e3
                TokenKind::Float(OrderedFloat(0.0002)),   // 2.0E-4
                TokenKind::Float(OrderedFloat(3e10)),     // 3e10
                TokenKind::Float(OrderedFloat(-0.0015)),  // -1.5e-3
                TokenKind::Float(OrderedFloat(400000.0)), // 4E+5
                TokenKind::Float(OrderedFloat(5.0)),      // 5e0
            ]
        );
    }

    #[test]
    pub fn test_strings() {
        let tokens = Tokenizer::from_code("test.rock", "\"hello\" 'world'")
            .tokenize_kinds()
            .unwrap();

        assert_eq!(
            tokens,
            vec![
                TokenKind::String("hello".into()),
                TokenKind::String("world".into()),
            ]
        );
    }

    #[test]
    pub fn test_keywords() {
        let tokens = Tokenizer::from_code("test.rock", "fn if else return null true false")
            .tokenize_kinds()
            .unwrap();

        assert_eq!(
            tokens,
            vec![
                TokenKind::Fn,
                TokenKind::If,
                TokenKind::Else,
                TokenKind::Return,
                TokenKind::Null,
                TokenKind::True,
                TokenKind::False,
            ]
        );
    }

    /// Every entry in `rock_tokenizer::token::KEYWORDS` must be recognized
    /// by the tokenizer as a non-Ident token. Catches drift when
    /// `KEYWORDS` adds a name the tokenizer doesn't recognize. The
    /// reverse direction (new keyword in the tokenizer match missing
    /// from `KEYWORDS`) is a doc-comment invariant — see the const's
    /// definition site.
    #[test]
    fn tokenizer_keyword_round_trip() {
        for kw in crate::token::KEYWORDS {
            let tokens = Tokenizer::from_code("test.rock", kw)
                .tokenize_kinds()
                .unwrap_or_else(|e| panic!("keyword {kw:?} failed to tokenize: {e:?}"));
            assert_eq!(tokens.len(), 1, "keyword {kw:?} produced {tokens:?}");
            match &tokens[0] {
                TokenKind::Ident(_) => panic!(
                    "{kw:?} listed in rock_tokenizer::token::KEYWORDS but tokenizer treats it as an identifier"
                ),
                _ => {}
            }
        }
    }

    #[test]
    fn test_num_method() {
        let tokens = Tokenizer::from_code("test.rock", "5.double()")
            .tokenize_kinds()
            .unwrap();

        assert_eq!(
            tokens,
            vec![
                TokenKind::Int(5, "5".into()),
                TokenKind::Dot,
                TokenKind::Ident("double".into()),
                TokenKind::OpenParen,
                TokenKind::CloseParen,
            ]
        );
    }

    #[test]
    fn test_range_tokenization() {
        let tokens = Tokenizer::from_code("test.rock", "0..5")
            .tokenize_kinds()
            .unwrap();

        assert_eq!(
            tokens,
            vec![
                TokenKind::Int(0, "0".into()),
                TokenKind::DoubleDot,
                TokenKind::Int(5, "5".into()),
            ]
        );
    }

    #[test]
    fn count_stats_classifies_string_double_slash_as_code() {
        // Regression: byte-walker counted `//` inside a string as a
        // comment. Token-based classifier sees the line carries a real
        // `StringLit` token and classifies it as code.
        let src = r#"let url = "https://example.com""#;
        let tokens = Tokenizer::from_code("t.rock", src).tokenize().unwrap();
        let stats = count_source_stats(src, &tokens);
        assert_eq!(stats.code_lines, 1, "URL string line must be code");
        assert_eq!(stats.comment_lines, 0);
    }

    #[test]
    fn count_stats_block_comment_open_inside_string_does_not_leak() {
        // Regression: byte-walker saw `/*` inside a string literal and
        // stayed in block-comment mode across the rest of the file.
        let src = "let s = \"/*\"\nlet x = 1\n";
        let tokens = Tokenizer::from_code("t.rock", src).tokenize().unwrap();
        let stats = count_source_stats(src, &tokens);
        assert_eq!(stats.code_lines, 2, "both lines are code");
        assert_eq!(stats.comment_lines, 0);
    }

    #[test]
    fn count_stats_basic_mix() {
        let src = "\
fn main() {       // trailing comment
    let x = 1
    // pure comment

    /* block */
}
";
        let tokens = Tokenizer::from_code("t.rock", src).tokenize().unwrap();
        let stats = count_source_stats(src, &tokens);
        assert_eq!(stats.files, 1);
        assert_eq!(stats.total_lines, 6);
        // `fn main() { // ...`, `let x = 1`, `}` -> 3 code lines
        // `// pure comment`, `/* block */` -> 2 comment-only lines
        // blank line -> 1
        assert_eq!(stats.code_lines, 3);
        assert_eq!(stats.comment_lines, 2);
        assert_eq!(stats.blank_lines, 1);
    }

    #[test]
    pub fn test_comments() {
        let tokens = Tokenizer::from_code("test.rock", "fn // hello\n if /* world */ (1 + 3)")
            .tokenize_kinds()
            .unwrap();

        assert_eq!(
            tokens,
            vec![
                TokenKind::Fn,
                TokenKind::Comment(" hello".into()),
                TokenKind::If,
                TokenKind::Comment(" world ".into()),
                TokenKind::OpenParen,
                TokenKind::Int(1, "1".into()),
                TokenKind::Plus,
                TokenKind::Int(3, "3".into()),
                TokenKind::CloseParen
            ]
        );
    }

    #[test]
    fn test_let_tokenization() {
        let tokens = Tokenizer::from_code("test.rock", "let a 5 = 4")
            .tokenize()
            .unwrap();

        let kinds = tokens.iter().map(|t| t.kind.clone()).collect_vec();

        assert_eq!(
            kinds,
            vec![
                TokenKind::Let,
                TokenKind::Ident("a".into()),
                TokenKind::Int(5, "5".into()),
                TokenKind::Assign,
                TokenKind::Int(4, "4".into()),
            ]
        );

        // tokens[4].span.print_debug("IDENT IN TOK");
    }

    #[test]
    fn test_mut_error() {
        let result = Tokenizer::from_code("test.rock", "let mut x = 5").tokenize();

        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error
            .message
            .contains("`let mut` is not supported, all bindings are mut, use just `let`"));
    }

    #[test]
    fn test_range_with_arithmetic_no_spaces() {
        // Test the fix for: 2..LEVEL_WIDTH-2 should tokenize correctly
        let tokens = Tokenizer::from_code("test.rock", "2..WIDTH-2")
            .tokenize()
            .unwrap();

        assert_eq!(tokens.len(), 5);
        assert_eq!(tokens[0].kind, TokenKind::Int(2, "2".into()));
        assert_eq!(tokens[1].kind, TokenKind::DoubleDot);
        assert_eq!(tokens[2].kind, TokenKind::Ident("WIDTH".into()));
        assert_eq!(tokens[3].kind, TokenKind::Minus);
        assert_eq!(tokens[4].kind, TokenKind::Int(2, "2".into()));
    }

    #[test]
    fn test_range_with_multiple_arithmetic() {
        // Test more complex cases: 0..SIZE*2-1
        let tokens = Tokenizer::from_code("test.rock", "0..SIZE*2-1")
            .tokenize()
            .unwrap();

        assert_eq!(tokens.len(), 7);
        assert_eq!(tokens[0].kind, TokenKind::Int(0, "0".into()));
        assert_eq!(tokens[1].kind, TokenKind::DoubleDot);
        assert_eq!(tokens[2].kind, TokenKind::Ident("SIZE".into()));
        assert_eq!(tokens[3].kind, TokenKind::Mul);
        assert_eq!(tokens[4].kind, TokenKind::Int(2, "2".into()));
        assert_eq!(tokens[5].kind, TokenKind::Minus);
        assert_eq!(tokens[6].kind, TokenKind::Int(1, "1".into()));
    }

    #[test]
    fn test_negative_numbers_in_appropriate_contexts() {
        // Test that negative numbers still work in appropriate contexts
        let test_cases = vec![
            (
                "x = -5",
                vec![
                    TokenKind::Ident("x".into()),
                    TokenKind::Assign,
                    TokenKind::Int(-5, "-5".into()),
                ],
            ),
            (
                "(-10)",
                vec![
                    TokenKind::OpenParen,
                    TokenKind::Int(-10, "-10".into()),
                    TokenKind::CloseParen,
                ],
            ),
            (
                "[-1, -2]",
                vec![
                    TokenKind::OpenSquare,
                    TokenKind::Int(-1, "-1".into()),
                    TokenKind::Comma,
                    TokenKind::Int(-2, "-2".into()),
                    TokenKind::CloseSquare,
                ],
            ),
        ];

        for (input, expected_tokens) in test_cases {
            let tokens = Tokenizer::from_code("test.rock", input).tokenize().unwrap();

            for (i, expected) in expected_tokens.iter().enumerate() {
                assert_eq!(
                    tokens[i].kind, *expected,
                    "Failed for input '{}' at token index {}",
                    input, i
                );
            }
        }
    }

    #[test]
    fn test_minus_as_binary_operator_after_literals() {
        // Test that minus is correctly treated as binary operator after various literals
        let test_cases = vec![
            (
                "5-3",
                vec![
                    TokenKind::Int(5, "5".into()),
                    TokenKind::Minus,
                    TokenKind::Int(3, "3".into()),
                ],
            ),
            (
                "x-1",
                vec![
                    TokenKind::Ident("x".into()),
                    TokenKind::Minus,
                    TokenKind::Int(1, "1".into()),
                ],
            ),
            (
                "true-false",
                vec![TokenKind::True, TokenKind::Minus, TokenKind::False],
            ),
            (
                "null-10",
                vec![
                    TokenKind::Null,
                    TokenKind::Minus,
                    TokenKind::Int(10, "10".into()),
                ],
            ),
        ];

        for (input, expected_tokens) in test_cases {
            let tokens = Tokenizer::from_code("test.rock", input).tokenize().unwrap();

            for (i, expected) in expected_tokens.iter().enumerate() {
                assert_eq!(
                    tokens[i].kind, *expected,
                    "Failed for input '{}' at token index {}",
                    input, i
                );
            }
        }
    }
}
