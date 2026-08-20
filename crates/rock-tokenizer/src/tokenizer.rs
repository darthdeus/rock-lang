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
        let raw = self.input.as_bytes();
        let base = self.byte_index;
        let fast_ok = base + n < raw.len()
            && raw[base..base + n].iter().all(|b| b.is_ascii());
        if fast_ok {
            let ahead = raw[base + n];
            if ahead.is_ascii() {
                return Some(ahead as char);
            }
            return decode_at(raw, base + n).map(|(got, _)| got);
        }
        self.input.get(base..)?.chars().nth(n)
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

    /// Read the string literal starting at `start` (the opening delimiter's
    /// first byte), advancing the cursor past it. One reader for every flavor --
    /// `"`, `"""`, `r"`, `r"""`, `'` -- over the shared `interp::scan_string`,
    /// so the terminator rule the lexer applies is the same one the parser and
    /// the LSP see. The cursor may already sit anywhere from `start` up to the
    /// literal's end (the `"` fast path pre-consumes the opening quote).
    fn read_string_literal(&mut self, start: usize) -> Result<TokenKind, CompilerError> {
        let open_span = self.start_span();
        let scan = crate::interp::scan_string(self.input, start);
        // Only `"..."` raises on an unclosed hole: there a bare `{` makes the
        // next `"` non-closing, so the literal would swallow the rest of the
        // file. `"""` closes on its own delimiter regardless of brace depth, so
        // a stray `{` is just text -- which is exactly why `inline(lua)` bodies
        // (full of braces) are written triple-quoted.
        let unclosed_hole = scan.flavor == crate::interp::Flavor::Quoted
            && matches!(
                scan.pieces.last(),
                Some(crate::interp::Piece::Hole { closed: false, .. })
            );
        // The actionable hint steers toward `{{` for a literal open-brace, which
        // is the only way in Rock to emit one.
        if !scan.terminated {
            self.advance_to(scan.end);
            return Err(CompilerError::new_with_label(
                self.end_span(open_span),
                match scan.flavor {
                    crate::interp::Flavor::Triple => {
                        "unterminated multiline string".to_string()
                    }
                    crate::interp::Flavor::RawTriple => {
                        "unterminated multiline raw string".to_string()
                    }
                    crate::interp::Flavor::Raw => "unterminated raw string".to_string(),
                    _ => "unterminated string literal: a bare `{` opens an interpolation \
                          that must be closed with `}`; use `{{` to emit a literal `{`"
                        .to_string(),
                },
            ));
        }
        if unclosed_hole {
            self.advance_to(scan.end);
            return Err(CompilerError::new_with_label(
                self.end_span(open_span),
                "unmatched `{` in string literal: use `{{` to emit a literal `{`"
                    .to_string(),
            ));
        }
        // After the structural refusals above, so an unterminated literal is
        // still reported as unterminated rather than as whatever escape its
        // runaway body happened to contain first.
        if let Some(bad) = scan.bad_escapes.first() {
            let (at, len, message) = (bad.at, bad.len, bad.message.clone());
            // Walk to the escape so the label carries its real line/column,
            // then to its end, then past the literal: advance_to keeps
            // line/column in step, so the cursor is where the caller expects.
            self.advance_to(at);
            let esc_start = self.start_span();
            self.advance_to(at + len);
            let span = self.end_span(esc_start);
            self.advance_to(scan.end);
            return Err(CompilerError::new_with_label(span, message));
        }
        let content = scan.content(self.input);
        self.advance_to(scan.end);
        Ok(match scan.flavor {
            crate::interp::Flavor::Raw | crate::interp::Flavor::RawTriple => {
                TokenKind::RawString(content.into())
            }
            _ => TokenKind::String(content.into()),
        })
    }

    /// Advance the cursor to byte index `target`, keeping line/column in step.
    fn advance_to(&mut self, target: usize) {
        while self.byte_index < target && self.current.is_some() {
            self.advance();
        }
    }

    fn read_keyword_or_ident(&mut self) -> Result<TokenKind, CompilerError> {
        if self.current == Some('r') && self.peek_n(1) == Some('"') {
            return self.read_string_literal(self.byte_index);
        }

        let ident = self.read_ident();

        match ident {
            "fn" => Ok(TokenKind::Fn),
            "operator" => Ok(TokenKind::Operator),
            "table" => Ok(TokenKind::Table),
            "type" => Ok(TokenKind::Type),
            "as" => Ok(TokenKind::As),

            "yield" => Ok(TokenKind::Yield),
            "await" => Ok(TokenKind::Await),

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
        if self.current.is_some_and(|c| c.is_ascii_digit()) {
            return true;
        }
        if self.at('-') {
            let after = self.peek_n(1);
            let numeric = match after {
                Some(d) if d.is_ascii_digit() => true,
                Some('.') => self.peek_n(2).is_some_and(|c| c.is_ascii_digit()),
                _ => false,
            };
            return numeric && self.minus_can_start_number();
        }
        if self.at('.') {
            return self.peek_n(1).is_some_and(|c| c.is_ascii_digit())
                && self.minus_can_start_number();
        }
        false
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
        enum Radix {
            Decimal,
            Binary,
            Hex,
        }
        let origin = self.start_span();
        let mut digits = String::with_capacity(16);
        let mut kept = String::with_capacity(16);
        let mut dot_seen = false;
        let mut floaty = false;
        let mut radix = Radix::Decimal;

        if self.at('-') {
            digits.push('-');
            kept.push('-');
            self.advance();
        }

        if self.at('0') {
            match self.peek_n(1) {
                Some('b') | Some('B') => {
                    kept.push_str("0b");
                    self.advance();
                    self.advance();
                    radix = Radix::Binary;
                }
                Some('x') | Some('X') => {
                    kept.push_str("0x");
                    self.advance();
                    self.advance();
                    radix = Radix::Hex;
                }
                Some(_) => {
                    digits.push('0');
                    kept.push('0');
                    self.advance();
                }
                None => {
                    digits.push('0');
                    kept.push('0');
                    self.advance();
                }
            }
        }

        match radix {
            Radix::Binary => {
            loop {
                let Some(cur) = self.current else { break };
                match cur {
                    '0' | '1' => {
                        digits.push(cur);
                        kept.push(cur);
                        self.advance();
                    }
                    '_' => {
                        kept.push(cur);
                        self.advance();
                    }
                    _ => break,
                }
            }

            if digits.is_empty() {
                return Err(CompilerError::new_with_label(
                    self.end_span(origin.clone()),
                    "Invalid binary literal: no digits after 0b".to_string(),
                )
                .with_code(CompilerErrorCode::TokenizerError));
            }

            let value = i64::from_str_radix(&digits, 2).map_err(|e| {
                CompilerError::new_with_label(
                    self.end_span(origin.clone()),
                    format!("Invalid binary literal '{}': {}", digits, e),
                )
                .with_code(CompilerErrorCode::TokenizerError)
            })?;
            Ok(TokenKind::Int(value, kept.into()))
            }
            Radix::Hex => {
            loop {
                let Some(cur) = self.current else { break };
                if cur.is_ascii_hexdigit() {
                    digits.push(cur);
                    kept.push(cur);
                    self.advance();
                } else if cur == '_' {
                    kept.push(cur);
                    self.advance();
                } else {
                    break;
                }
            }

            if digits.is_empty() {
                return Err(CompilerError::new_with_label(
                    self.end_span(origin.clone()),
                    "Invalid hex literal: no digits after 0x".to_string(),
                )
                .with_code(CompilerErrorCode::TokenizerError));
            }

            let value = i64::from_str_radix(&digits, 16).map_err(|e| {
                CompilerError::new_with_label(
                    self.end_span(origin.clone()),
                    format!("Invalid hex literal '{}': {}", digits, e),
                )
                .with_code(CompilerErrorCode::TokenizerError)
            })?;
            Ok(TokenKind::Int(value, kept.into()))
            }
            Radix::Decimal => {
            loop {
                let Some(cur) = self.current else { break };
                match cur {
                    '0'..='9' => {
                        digits.push(cur);
                        kept.push(cur);
                        self.advance();
                        continue;
                    }
                    '.' => {
                        if dot_seen {
                            break;
                        }
                        if matches!(&self.last_token_kind, Some(TokenKind::Dot)) {
                            break;
                        }
                        if !self.peek_n(1).is_some_and(|c| c.is_ascii_digit()) {
                            break;
                        }
                        floaty = true;
                        digits.push(cur);
                        kept.push(cur);
                        dot_seen = true;
                        self.advance();
                        continue;
                    }
                    'e' | 'E' => {
                        floaty = true;
                        digits.push(cur);
                        kept.push(cur);
                        self.advance();
                        if let Some(sign) = self.current {
                            if sign == '+' || sign == '-' {
                                digits.push(sign);
                                kept.push(sign);
                                self.advance();
                            }
                        }
                        let mut exp_digits = false;
                        loop {
                            let Some(ec) = self.current else { break };
                            if !ec.is_ascii_digit() {
                                break;
                            }
                            digits.push(ec);
                            kept.push(ec);
                            exp_digits = true;
                            self.advance();
                        }
                        if !exp_digits {
                            return Err(CompilerError::new_with_label(
                                self.end_span(origin.clone()),
                                format!("Invalid scientific notation: expected digits after exponent in '{}'", kept),
                            )
                            .with_code(CompilerErrorCode::TokenizerError));
                        }
                        break;
                    }
                    _ => break,
                }
            }

            Ok(if floaty {
                let parsed_float = digits.parse().map_err(|_| {
                    CompilerError::new_with_label(
                        self.end_span(origin.clone()),
                        format!("Invalid floating point literal: '{}'", digits),
                    )
                    .with_code(CompilerErrorCode::TokenizerError)
                })?;
                TokenKind::Float(parsed_float)
            } else {
                let parsed_int = digits.parse::<i64>().map_err(|e| {
                    CompilerError::new_with_label(
                        self.end_span(origin.clone()),
                        format!("Invalid integer literal '{}': {}", digits, e),
                    )
                    .with_code(CompilerErrorCode::TokenizerError)
                })?;
                TokenKind::Int(parsed_int, kept.into())
            })
            }
        }
    }

    pub fn next_token(&mut self) -> Result<Option<Token>, CompilerError> {
        self.skip_whitespace();
        let origin = self.start_span();
        let Some(head) = self.current else {
            return Ok(None);
        };
        if head.is_ascii_alphabetic() || head == '_' {
            let kind = self.read_keyword_or_ident()?;
            return self.finish_token(origin, kind);
        }
        if head.is_ascii_digit()
            || ((head == '-' || head == '.') && self.lookahead_number())
        {
            let kind = self.read_number()?;
            return self.finish_token(origin, kind);
        }
        if let Some(kind) = single_char_token(head) {
            self.advance();
            return self.finish_token(origin, kind);
        }
        let kind: TokenKind = match self.current {
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
                self.advance();
                if self.eat('.') {
                    if self.eat('.') {
                        TokenKind::TripleDot
                    } else {
                        TokenKind::DoubleDot
                    }
                } else if self.eat('/') {
                    TokenKind::IntDiv
                } else {
                    TokenKind::Dot
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
                let lit_at = self.byte_index;
                if self.peek_n(1) == Some('"') && self.peek_n(2) == Some('"') {
                    self.read_string_literal(lit_at)?
                } else {
                    // Verbatim slice applies only without escapes or holes.
                    self.advance();
                    if let Some(body) = self.try_read_plain_string() {
                        TokenKind::String(body.into())
                    } else {
                        self.read_string_literal(lit_at)?
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

            Some('\'') => self.read_string_literal(self.byte_index)?,

            Some('#') => {
                match self.peek_n(1) {
                    Some(nxt) if nxt.is_ascii_lowercase() => {
                        self.advance();
                        let src = self.input;
                        let word_at = self.byte_index;
                        {
                            let raw = src.as_bytes();
                            let mut j = self.byte_index;
                            while j < raw.len() && raw[j].is_ascii_alphabetic() {
                                j += 1;
                            }
                            let took = j - self.byte_index;
                            self.byte_index = j;
                            self.byte_offset += took;
                            self.column += took;
                            self.current = decode_at(raw, j).map(|(got, _)| got);
                        }
                        match &src[word_at..self.byte_index] {
                            "if" => TokenKind::DirectiveIf,
                            "elseif" => TokenKind::DirectiveElseIf,
                            "else" => TokenKind::DirectiveElse,
                            "end" => TokenKind::DirectiveEnd,
                            _ => {
                                let _ = self.read_line_rest();
                                TokenKind::Comment(src[word_at..self.byte_index].into())
                            }
                        }
                    }
                    Some(_) => {
                        self.advance();
                        TokenKind::Comment(self.read_line_rest().into())
                    }
                    None => {
                        self.advance();
                        TokenKind::Comment(String::new().into())
                    }
                }
            }

            _ => {
                return Err(CompilerError::new_with_label(
                    Span::unknown(),
                    format!("I done a bad thing: {:?}", self.current),
                ))
            }
        };

        self.finish_token(origin, kind)
    }

    fn finish_token(
        &mut self,
        origin: LineCol,
        kind: TokenKind,
    ) -> Result<Option<Token>, CompilerError> {
        let meaningful = !matches!(kind, TokenKind::Newline | TokenKind::Comment(_));
        if meaningful {
            self.last_token_kind = Some(kind.clone());
        }
        self.just_skipped_whitespace = false;
        Ok(Some(Token {
            kind,
            span: self.end_span(origin),
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
        Span::from_linecol(self.start_span())
    }

    pub fn tokenize(&mut self) -> Result<Vec<Token>, CompilerError> {
        let mut out: Vec<Token> = Vec::with_capacity(self.input.len() / 5 + 8);
        loop {
            let nxt = self.next_token()?;
            let Some(tok) = nxt else { break };
            if tok.kind == TokenKind::Newline {
                if let Some(prev) = out.last_mut() {
                    prev.trailing_newlines += 1;
                }
            } else {
                out.push(tok)
            }
        }
        Ok(out)
    }

    pub fn tokenize_kinds(&mut self) -> Result<Vec<TokenKind>, CompilerError> {
        let all = self.tokenize()?;
        let mut kinds = Vec::with_capacity(all.len());
        for tok in all {
            kinds.push(tok.kind);
        }
        Ok(kinds)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_number_tokenization() {
        let tokens = Tokenizer::from_code("test.rock", "1 23 456 0.4")
            .tokenize_kinds()
            .unwrap();

        assert_eq!(
            tokens,
            vec![
                TokenKind::Int(1, "1".into()),
                TokenKind::Int(23, "23".into()),
                TokenKind::Int(456, "456".into()),
                TokenKind::Float(OrderedFloat(0.4)),
            ]
        );
    }

    #[test]
    fn test_trailing_dot_is_not_a_float() {
        // A `.` is a decimal point only when a digit follows. Trailing-dot
        // floats (`456.`) are removed so `2./4` lexes as `2 ./ 4`.
        assert_eq!(
            Tokenizer::from_code("t.rock", "456.").tokenize_kinds().unwrap(),
            vec![TokenKind::Int(456, "456".into()), TokenKind::Dot]
        );
        assert_eq!(
            Tokenizer::from_code("t.rock", "2./4").tokenize_kinds().unwrap(),
            vec![
                TokenKind::Int(2, "2".into()),
                TokenKind::IntDiv,
                TokenKind::Int(4, "4".into()),
            ]
        );
        // Spaced form and decimal points are unaffected.
        assert_eq!(
            Tokenizer::from_code("t.rock", "7 ./ 2").tokenize_kinds().unwrap(),
            vec![
                TokenKind::Int(7, "7".into()),
                TokenKind::IntDiv,
                TokenKind::Int(2, "2".into()),
            ]
        );
        assert_eq!(
            Tokenizer::from_code("t.rock", "2.5").tokenize_kinds().unwrap(),
            vec![TokenKind::Float(OrderedFloat(2.5))]
        );
    }

    #[test]
    fn test_leading_dot_is_a_float() {
        // A bare `.5` in expression-start position is a float literal (`0.5`).
        assert_eq!(
            Tokenizer::from_code("t.rock", ".35").tokenize_kinds().unwrap(),
            vec![TokenKind::Float(OrderedFloat(0.35))]
        );
        // Every prefix position where a unary minus is allowed also admits a
        // leading-dot float.
        assert_eq!(
            Tokenizer::from_code("t.rock", "restitution: .35").tokenize_kinds().unwrap(),
            vec![
                TokenKind::Ident("restitution".into()),
                TokenKind::Colon,
                TokenKind::Float(OrderedFloat(0.35)),
            ]
        );
        assert_eq!(
            Tokenizer::from_code("t.rock", "[.5, .25]").tokenize_kinds().unwrap(),
            vec![
                TokenKind::OpenSquare,
                TokenKind::Float(OrderedFloat(0.5)),
                TokenKind::Comma,
                TokenKind::Float(OrderedFloat(0.25)),
                TokenKind::CloseSquare,
            ]
        );
        assert_eq!(
            Tokenizer::from_code("t.rock", "2 + .5").tokenize_kinds().unwrap(),
            vec![
                TokenKind::Int(2, "2".into()),
                TokenKind::Plus,
                TokenKind::Float(OrderedFloat(0.5)),
            ]
        );
        // `-.5` stays a negative float (unchanged).
        assert_eq!(
            Tokenizer::from_code("t.rock", "-.5").tokenize_kinds().unwrap(),
            vec![TokenKind::Float(OrderedFloat(-0.5))]
        );
        // Tuple/field index `.0` after a value stays Dot + Int, NOT a float.
        assert_eq!(
            Tokenizer::from_code("t.rock", "w.0").tokenize_kinds().unwrap(),
            vec![
                TokenKind::Ident("w".into()),
                TokenKind::Dot,
                TokenKind::Int(0, "0".into()),
            ]
        );
        assert_eq!(
            Tokenizer::from_code("t.rock", "w.0.0").tokenize_kinds().unwrap(),
            vec![
                TokenKind::Ident("w".into()),
                TokenKind::Dot,
                TokenKind::Int(0, "0".into()),
                TokenKind::Dot,
                TokenKind::Int(0, "0".into()),
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
        let tokens = Tokenizer::from_code("test.rock", "-1 2.3 -7 -.3")
            .tokenize_kinds()
            .unwrap();

        assert_eq!(
            tokens,
            vec![
                TokenKind::Int(-1, "-1".into()),
                TokenKind::Float(OrderedFloat(2.3)),
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

#[cfg(test)]
mod escape_tests {
    use super::*;

    fn lex(src: &str) -> Result<Vec<TokenKind>, CompilerError> {
        Tokenizer::from_code("t.rock", src).tokenize_kinds()
    }

    fn text(src: &str) -> String {
        match lex(src).expect("lexes").into_iter().next().expect("one token") {
            TokenKind::String(s) | TokenKind::RawString(s) => s.to_string(),
            other => panic!("not a string token: {other:?}"),
        }
    }

    fn rejection(src: &str) -> String {
        lex(src).expect_err("must reject").message
    }

    #[test]
    fn unicode_escapes_resolve_to_their_scalar() {
        assert_eq!(text(r#""\u{41}""#), "A");
        assert_eq!(text(r#""M\u{00c1}G""#), "M\u{00c1}G");
        assert_eq!(text(r#""\u{1F600}""#), "\u{1F600}");
        // Hex is case-insensitive and 1..=6 digits, so the short and long
        // spellings of one scalar agree.
        assert_eq!(text(r#""\u{c1}""#), text(r#""\u{0000C1}""#));
    }

    // The emitted bytes are what a charset declaration is read as, so the
    // escape has to produce UTF-8 rather than a lone char in some other width.
    #[test]
    fn an_astral_escape_emits_four_utf8_bytes() {
        let s = text(r#""\u{1F600}""#);
        assert_eq!(s.len(), 4, "U+1F600 is four UTF-8 bytes");
        assert_eq!(s.chars().count(), 1);
    }

    #[test]
    fn carriage_return_is_an_escape_rather_than_the_letter_r() {
        assert_eq!(text(r#""a\rb""#), "a\rb");
        assert_eq!(text(r#""\r""#).as_bytes(), &[13]);
    }

    #[test]
    fn the_known_escapes_still_resolve() {
        assert_eq!(text(r#""a\nb\tc""#), "a\nb\tc");
        assert_eq!(text(r#""q\"q""#), "q\"q");
        assert_eq!(text(r#""b\\b""#), "b\\b");
    }

    // Silently dropping the backslash is what let `stdlib/json.rock` decode a
    // JSON `\r` as the letter `r`. An unknown escape is a rejection, never a
    // string the author did not write.
    #[test]
    fn an_unknown_escape_is_rejected() {
        assert!(rejection(r#""\q""#).contains("unknown string escape"));
        assert!(rejection(r#""C:\path""#).contains("unknown string escape"));
        // The message names the raw-string escape hatch, which is the fix for
        // every Windows path and regex in the corpus.
        assert!(rejection(r#""\d+""#).contains("r\"..\""));
    }

    #[test]
    fn a_malformed_unicode_escape_is_rejected() {
        assert!(rejection(r#""\u41""#).contains("\\u{"), "braces are required");
        assert!(rejection(r#""\u{}""#).contains("at least one"));
        assert!(rejection(r#""\u{41""#).contains("unterminated"));
        assert!(rejection(r#""\u{XY}""#).contains("hex"));
        assert!(rejection(r#""\u{1234567}""#).contains("at most six"));
    }

    // Both ends of the scalar range, so the check is not a one-sided bound.
    #[test]
    fn a_non_scalar_codepoint_is_rejected() {
        assert!(rejection(r#""\u{D800}""#).contains("surrogate"));
        assert!(rejection(r#""\u{DFFF}""#).contains("surrogate"));
        assert!(rejection(r#""\u{110000}""#).contains("above"));
        // And the neighbours that ARE scalars still lex, so the rejection is
        // narrow rather than a range that swallowed them.
        assert_eq!(text(r#""\u{D7FF}""#), "\u{D7FF}");
        assert_eq!(text(r#""\u{E000}""#), "\u{E000}");
        assert_eq!(text(r#""\u{10FFFF}""#), "\u{10FFFF}");
    }

    // Raw and triple-quoted flavors do not run the escape pass at all, so they
    // are the escape hatch AND cannot be broken by a stricter one.
    #[test]
    fn flavors_without_an_escape_pass_are_untouched() {
        assert_eq!(text("r\"\\d+\""), "\\d+");
        assert_eq!(text("r\"C:\\path\\to\""), "C:\\path\\to");
        assert_eq!(text("\"\"\"\\d+\"\"\""), "\\d+");
    }

    // A wrapped literal is ONE line of text: the newline and the next line's
    // indentation are both consumed. Dropping only the backslash left both in
    // the string, which is what every wrapped message in the corpus was getting.
    #[test]
    fn a_backslash_before_a_newline_continues_the_line() {
        assert_eq!(text("\"one \\\n         two\""), "one two");
        assert_eq!(text("\"one \\\n\ttwo\""), "one two");
        // Only leading horizontal space is eaten -- an intentional blank line
        // still needs `\n`.
        assert_eq!(text("\"a\\\n  b\\nc\""), "ab\nc");
    }

    #[test]
    fn an_escape_inside_an_interpolation_hole_still_resolves() {
        // The hole text is kept verbatim in the token; the escape pass runs
        // when the hole is re-parsed, so the token keeps the source spelling.
        assert_eq!(text(r#""x={f("\u{41}")}""#), "x={f(\"\\u{41}\")}");
    }
}
