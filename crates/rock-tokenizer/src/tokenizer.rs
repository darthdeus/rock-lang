use crate::prelude::*;

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    pub trailing_newlines: usize,
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

/// Spans hold positions as `u32`, so a source whose last byte offset does
/// not fit is refused before any span is made from it. Lines and columns
/// never exceed the byte offset, so this one check covers them too.
pub fn source_fits_spans(file: Ustr, start_offset: usize, len: usize) -> Result<(), CompilerError> {
    match start_offset.checked_add(len) {
        Some(end) if end <= u32::MAX as usize => Ok(()),
        _ => Err(CompilerError::new_no_location(format!(
            "{file} is too large: {len} bytes; Rock sources are limited to {} bytes",
            u32::MAX
        ))),
    }
}

macro_rules! lua_keywords {
    ($($word:literal),* $(,)?) => {
        fn escape_lua_keyword(name: &str) -> &str {
            match name {
                $($word => concat!("__rock_", $word),)*
                _ => name,
            }
        }

        fn unescape_lua_keyword(name: &str) -> &str {
            match name {
                $(concat!("__rock_", $word) => $word,)*
                _ => name,
            }
        }
    };
}

lua_keywords!(
    "and", "do", "elseif", "end", "function", "goto", "local", "nil", "not", "or", "repeat",
    "then", "until"
);

impl<'a> Tokenizer<'a> {
    pub fn new(input: &'a str, source_file: Ustr) -> Self {
        Self::new_with_offset(input, source_file, 0, 0, 0)
    }

    /// Tokenize a fragment whose first byte is at this position in its source file.
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
        Self::new(input, id_path.into())
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

        // Byte offset into the source buffer — matches `Span.offset_range()`
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

    pub(crate) fn peek_n(&self, n: usize) -> Option<char> {
        let raw = self.input.as_bytes();
        let base = self.byte_index;
        let fast_ok = base + n < raw.len() && raw[base..base + n].iter().all(|b| b.is_ascii());
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
                    crate::interp::Flavor::Triple => "unterminated multiline string".to_string(),
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
                "unmatched `{` in string literal: use `{{` to emit a literal `{`".to_string(),
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

    /// Advance the cursor to byte index `target` (rounded up to a char
    /// boundary), keeping line/column in step as [`Self::advance`] would.
    fn advance_to(&mut self, target: usize) {
        let bytes = self.input.as_bytes();
        let mut end = target.min(bytes.len());
        while end < bytes.len() && bytes[end] & 0xC0 == 0x80 {
            end += 1;
        }
        if end <= self.byte_index {
            return;
        }
        let run = &bytes[self.byte_index..end];
        let chars = |s: &[u8]| s.iter().filter(|&&b| b & 0xC0 != 0x80).count();
        match run.iter().rposition(|&b| b == b'\n') {
            Some(last) => {
                self.line += run.iter().filter(|&&b| b == b'\n').count();
                self.column = chars(&run[last + 1..]);
            }
            None => self.column += chars(run),
        }
        self.byte_offset += run.len();
        self.byte_index = end;
        self.current = decode_at(bytes, end).map(|(c, _)| c);
    }

    fn read_keyword_or_ident(&mut self) -> Result<TokenKind, CompilerError> {
        if self.current == Some('r') && self.peek_n(1) == Some('"') {
            return self.read_string_literal(self.byte_index);
        }

        let ident = self.read_ident();

        if ident == "mut" {
            return Err(CompilerError::new_with_label(
                self.empty_span(),
                "`let mut` is not supported, all bindings are mut, use just `let`".to_string(),
            ));
        }
        Ok(TokenKind::keyword(ident)
            .unwrap_or_else(|| TokenKind::Ident(escape_lua_keyword(ident).into())))
    }

    /// Recover the source spelling of a Lua keyword used as a Rock identifier.
    pub fn unescape_lua_keyword(name: &str) -> &str {
        unescape_lua_keyword(name)
    }

    pub(crate) fn lookahead_number(&self) -> bool {
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

    fn minus_can_start_number(&self) -> bool {
        use TokenKind::*;
        self.just_skipped_whitespace
            || !matches!(
                self.last_token_kind,
                Some(
                    Ident(_)
                        | Int(_, _)
                        | Float(_)
                        | String(_)
                        | RawString(_)
                        | True
                        | False
                        | Null
                        | CloseParen
                        | CloseSquare
                        | CloseCurly
                )
            )
    }

    pub(crate) fn read_number(&mut self) -> Result<TokenKind, CompilerError> {
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
                while let Some(cur) = self.current {
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
                while let Some(cur) = self.current {
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
                while let Some(cur) = self.current {
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
                            while let Some(ec) = self.current {
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

    fn read_punctuation(&mut self, head: char) -> Option<TokenKind> {
        use TokenKind::*;
        let rest = &self.input.as_bytes()[self.byte_index + 1..];
        let (kind, len) = match (head, rest) {
            ('-', [b'>', ..]) => (Arrow, 2),
            ('-', [b'=', ..]) => (SubAssign, 2),
            ('-', [b'-', ..]) => (MinusMinus, 2),
            ('-', _) => (Minus, 1),
            ('+', [b'+', ..]) => (PlusPlus, 2),
            ('+', [b'=', ..]) => (AddAssign, 2),
            ('+', _) => (Plus, 1),
            ('*', [b'=', ..]) => (MulAssign, 2),
            ('*', _) => (Mul, 1),
            ('/', [b'=', ..]) => (DivAssign, 2),
            ('/', [b'/' | b'*', ..]) => return None,
            ('/', _) => (Div, 1),
            ('=', [b'=', ..]) => (Equals, 2),
            ('=', [b'>', ..]) => (FatArrow, 2),
            ('=', _) => (Assign, 1),
            ('!', [b'=', ..]) => (NotEquals, 2),
            ('!', _) => (Bang, 1),
            ('<', [b'=', ..]) => (LessThanEqual, 2),
            ('<', [b'<', ..]) => (LeftShift, 2),
            ('<', _) => (LessThan, 1),
            ('>', [b'=', ..]) => (GreaterThanEqual, 2),
            ('>', [b'>', ..]) => (RightShift, 2),
            ('>', _) => (GreaterThan, 1),
            (':', [b':', ..]) => (DoubleColon, 2),
            (':', _) => (Colon, 1),
            ('.', [b'.', b'.', ..]) => (TripleDot, 3),
            ('.', [b'.', ..]) => (DoubleDot, 2),
            ('.', [b'/', ..]) => (IntDiv, 2),
            ('.', _) => (Dot, 1),
            ('?', [b'?', b'?', ..]) => (TripleQuestionMark, 3),
            ('?', [b'?', ..]) => (DoubleQuestionMark, 2),
            ('?', [b'.', ..]) => (QuestionDot, 2),
            ('?', _) => (QuestionMark, 1),
            ('&', [b'&', ..]) => (And, 2),
            ('&', _) => (BitAnd, 1),
            ('|', [b'|', ..]) => (Or, 2),
            ('|', _) => (BitOr, 1),
            _ => return None,
        };
        self.byte_index += len;
        self.byte_offset += len;
        self.column += len;
        self.current = decode_at(self.input.as_bytes(), self.byte_index).map(|(c, _)| c);
        Some(kind)
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
        if head.is_ascii_digit() || ((head == '-' || head == '.') && self.lookahead_number()) {
            let kind = self.read_number()?;
            return self.finish_token(origin, kind);
        }
        if let Some(kind) = single_char_token(head) {
            self.advance();
            return self.finish_token(origin, kind);
        }
        if let Some(kind) = self.read_punctuation(head) {
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

            Some('/') => {
                self.advance();
                let start = self.byte_index + 1;
                if self.eat('/') {
                    TokenKind::Comment(self.read_line_rest().into())
                } else {
                    debug_assert!(self.at('*'));
                    let input = self.input;
                    let Some(len) = input[start..].find("*/") else {
                        self.advance_to(input.len());
                        return Err(CompilerError::new_with_label(
                            self.end_span(origin),
                            "unterminated block comment: `/*` has no closing `*/`".to_string(),
                        ));
                    };
                    let end = start + len;
                    self.advance_to(end + 2);
                    TokenKind::Comment(input[start..end].into())
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

            Some('\'') => self.read_string_literal(self.byte_index)?,

            Some('#') => match self.peek_n(1) {
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
            },

            _ => {
                return Err(CompilerError::new_with_label(
                    Span::unknown(),
                    format!("I done a bad thing: {:?}", self.current),
                ));
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
            self.last_token_kind = Some(kind);
        }
        self.just_skipped_whitespace = false;
        Ok(Some(Token {
            kind,
            span: self.end_span(origin),
            trailing_newlines: 0,
        }))
    }

    pub(crate) fn start_span(&self) -> LineCol {
        LineCol {
            file: self.source_file,
            line: self.line,
            col: self.column,
            offset: self.byte_offset,
        }
    }

    pub(crate) fn end_span(&self, start: LineCol) -> Span {
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

        Span::new(
            start.file,
            (start.line, self.line),
            (start.col, end_col),
            offset_range,
        )
    }

    pub fn empty_span(&self) -> Span {
        Span::from_linecol(self.start_span())
    }

    pub fn tokenize(&mut self) -> Result<Vec<Token>, CompilerError> {
        source_fits_spans(self.source_file, self.byte_offset, self.input.len())?;
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
    fn a_source_past_the_span_width_is_a_compile_error() {
        let file = ustr::ustr("big.rock");
        assert!(source_fits_spans(file, 0, u32::MAX as usize).is_ok());
        let err = source_fits_spans(file, 0, u32::MAX as usize + 1).unwrap_err();
        assert!(
            err.message.contains("big.rock is too large"),
            "{}",
            err.message
        );
        assert!(source_fits_spans(file, 1, u32::MAX as usize).is_err());
    }

    #[test]
    fn spans_after_slow_path_literals_count_lines_chars_and_bytes() {
        // `é` is 2 bytes / 1 char. The triple literal ends on line 1 after
        // `xé"""` (5 chars, 6 bytes) and a space, so `y` sits at line 1,
        // column 6, byte 13.
        let src = "\"\"\"é\nxé\"\"\" y \"a{b}é\" z";
        let tokens = Tokenizer::from_code("t.rock", src).tokenize().unwrap();
        let y = &tokens[1].span;
        assert_eq!(tokens[1].kind, TokenKind::Ident("y".into()));
        assert_eq!((y.line_range().0, y.col_range().0), (1, 6));
        assert_eq!(y.offset_range().0, 13);
        let z = &tokens[3].span;
        assert_eq!(tokens[3].kind, TokenKind::Ident("z".into()));
        assert_eq!((z.line_range().0, z.col_range().0), (1, 16));
        assert_eq!(z.offset_range().0, 24);
        // A bad escape on a later line is labelled at its own line and column.
        let err = Tokenizer::from_code("t.rock", "\"é\\\nab\\q\"")
            .tokenize()
            .unwrap_err();
        let span = err.labels[0].span;
        assert_eq!((span.line_range().0, span.col_range()), (1, (2, 4)));
    }
}

#[cfg(test)]
mod escape_tests {
    use super::*;

    fn lex(src: &str) -> Result<Vec<TokenKind>, CompilerError> {
        Tokenizer::from_code("t.rock", src).tokenize_kinds()
    }

    fn text(src: &str) -> String {
        match lex(src)
            .expect("lexes")
            .into_iter()
            .next()
            .expect("one token")
        {
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
        assert!(
            rejection(r#""\u41""#).contains("\\u{"),
            "braces are required"
        );
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
