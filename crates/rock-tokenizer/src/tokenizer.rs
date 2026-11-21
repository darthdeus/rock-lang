use crate::interp::{Flavor, Piece, scan_string};
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

    // Byte position in `input`, not a char index.
    byte_index: usize,

    last_token_kind: Option<TokenKind>,

    just_skipped_whitespace: bool,
}

#[inline(always)]
fn decode_at(input: &str, i: usize) -> Option<char> {
    let b = *input.as_bytes().get(i)?;
    if b.is_ascii() {
        return Some(b as char);
    }
    input[i..].chars().next()
}

fn is_char_start(b: u8) -> bool {
    b & 0xC0 != 0x80
}

pub(crate) fn directive(src: &str) -> Option<(TokenKind, usize)> {
    if !src.as_bytes().first().is_some_and(u8::is_ascii_lowercase) {
        return None;
    }
    let len = src.bytes().take_while(u8::is_ascii_alphabetic).count();
    let kind = match &src[..len] {
        "if" => TokenKind::DirectiveIf,
        "elseif" => TokenKind::DirectiveElseIf,
        "else" => TokenKind::DirectiveElse,
        "end" => TokenKind::DirectiveEnd,
        _ => return None,
    };
    Some((kind, len))
}

/// Lines and columns cannot exceed the byte offset, so one `u32` check covers all spans.
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
        let current = decode_at(input, 0);

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
        let Some(c) = self.current else { return };
        if c == '\n' {
            self.line += 1;
            self.column = 0;
        } else {
            self.column += 1;
        }
        self.set_byte_index(self.byte_index + c.len_utf8());
    }

    fn set_byte_index(&mut self, end: usize) {
        self.byte_offset += end - self.byte_index;
        self.byte_index = end;
        self.current = decode_at(self.input, end);
    }

    // Columns count chars, offsets count bytes.
    fn advance_on_line(&mut self, end: usize, chars: usize) {
        self.column += chars;
        self.set_byte_index(end);
    }

    fn peek_byte(&self, ahead: usize) -> Option<u8> {
        self.input.as_bytes().get(self.byte_index + ahead).copied()
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
        let start = self.byte_index;
        let len = self.input.as_bytes()[start..]
            .iter()
            .take_while(|&&b| b == b' ')
            .count();
        if len != 0 {
            self.advance_on_line(start + len, len);
            self.just_skipped_whitespace = true;
        }
    }

    fn read_ident(&mut self) -> &'a str {
        let bytes = self.input.as_bytes();
        let start = self.byte_index;
        let mut end = start;
        let mut chars = 0;
        while let Some(&b) = bytes.get(end) {
            end += if b.is_ascii_alphanumeric() || b == b'_' {
                1
            } else if !b.is_ascii() {
                match decode_at(self.input, end) {
                    Some(c) if c.is_alphanumeric() => c.len_utf8(),
                    _ => break,
                }
            } else {
                break;
            };
            chars += 1;
        }
        self.advance_on_line(end, chars);
        &self.input[start..end]
    }

    fn read_line_rest(&mut self) -> &'a str {
        let start = self.byte_index;
        let mut end = start;
        let mut chars = 0;
        for &b in &self.input.as_bytes()[start..] {
            if b == b'\n' {
                break;
            }
            end += 1;
            chars += usize::from(is_char_start(b));
        }
        self.advance_on_line(end, chars);
        &self.input[start..end]
    }

    /// Leaves the cursor in place when an escape, line break or interpolation needs the full scanner.
    fn try_read_plain_string(&mut self) -> Option<&'a str> {
        let start = self.byte_index;
        let mut chars = 0;
        for (i, &b) in self.input.as_bytes()[start..].iter().enumerate() {
            match b {
                b'"' => {
                    let end = start + i;
                    self.advance_on_line(end + 1, chars + 1);
                    return Some(&self.input[start..end]);
                }
                b'\\' | b'{' | b'\n' => return None,
                b => chars += usize::from(is_char_start(b)),
            }
        }
        None
    }

    fn read_string_literal(&mut self, start: usize) -> Result<TokenKind, CompilerError> {
        let open_span = self.start_span();
        let scan = scan_string(self.input, start);
        let unclosed_hole = scan.flavor == Flavor::Quoted
            && matches!(scan.pieces.last(), Some(Piece::Hole { closed: false, .. }));
        if !scan.terminated {
            self.advance_to(scan.end);
            return Err(CompilerError::new_with_label(
                self.end_span(open_span),
                match scan.flavor {
                    Flavor::Triple => "unterminated multiline string".to_string(),
                    Flavor::RawTriple => "unterminated multiline raw string".to_string(),
                    Flavor::Raw => "unterminated raw string".to_string(),
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
        // After the structural refusals, so an unterminated literal is reported as such.
        if let Some(bad) = scan.bad_escapes.first() {
            self.advance_to(bad.at);
            let esc_start = self.start_span();
            self.advance_to(bad.at + bad.len);
            let span = self.end_span(esc_start);
            self.advance_to(scan.end);
            return Err(CompilerError::new_with_label(span, bad.message.clone()));
        }
        let content = scan.content(self.input);
        self.advance_to(scan.end);
        Ok(match scan.flavor {
            Flavor::Raw | Flavor::RawTriple => TokenKind::RawString(content.into()),
            _ => TokenKind::String(content.into()),
        })
    }

    fn advance_to(&mut self, target: usize) {
        let bytes = self.input.as_bytes();
        let mut end = target.min(bytes.len());
        while end < bytes.len() && !is_char_start(bytes[end]) {
            end += 1;
        }
        if end <= self.byte_index {
            return;
        }
        let run = &bytes[self.byte_index..end];
        let chars = |s: &[u8]| s.iter().filter(|&&b| is_char_start(b)).count();
        match run.iter().rposition(|&b| b == b'\n') {
            Some(last) => {
                self.line += run.iter().filter(|&&b| b == b'\n').count();
                self.column = chars(&run[last + 1..]);
            }
            None => self.column += chars(run),
        }
        self.set_byte_index(end);
    }

    fn read_keyword_or_ident(&mut self) -> Result<TokenKind, CompilerError> {
        if self.current == Some('r') && self.peek_byte(1) == Some(b'"') {
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

    fn lookahead_number(&self, head: char) -> bool {
        let digit_at = |ahead| self.peek_byte(ahead).is_some_and(|b| b.is_ascii_digit());
        match head {
            '0'..='9' => true,
            '-' => {
                (digit_at(1) || (self.peek_byte(1) == Some(b'.') && digit_at(2)))
                    && self.minus_can_start_number()
            }
            '.' => digit_at(1) && self.minus_can_start_number(),
            _ => false,
        }
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

    fn read_number(&mut self) -> Result<TokenKind, CompilerError> {
        let origin = self.start_span();
        let start = self.byte_index;
        let negative = self.eat('-');
        if self.at('0') {
            match self.peek_byte(1) {
                Some(b'b' | b'B') => {
                    return self.read_radix_number(origin, negative, 2, "binary", "0b");
                }
                Some(b'x' | b'X') => {
                    return self.read_radix_number(origin, negative, 16, "hex", "0x");
                }
                _ => {}
            }
        }

        self.read_decimal_digits();
        let fraction = self.at('.')
            && self.last_token_kind != Some(TokenKind::Dot)
            && self.peek_byte(1).is_some_and(|b| b.is_ascii_digit());
        if fraction {
            self.advance();
            self.read_decimal_digits();
        }
        let exponent = matches!(self.current, Some('e' | 'E'));
        if exponent {
            self.advance();
            if matches!(self.current, Some('+' | '-')) {
                self.advance();
            }
            let exp_start = self.byte_index;
            self.read_decimal_digits();
            if self.byte_index == exp_start {
                let literal = &self.input[start..exp_start];
                return Err(self.number_error(
                    origin,
                    format!(
                        "Invalid scientific notation: expected digits after exponent in '{literal}'"
                    ),
                ));
            }
        }

        let literal = &self.input[start..self.byte_index];
        if fraction || exponent {
            literal.parse().map(TokenKind::Float).map_err(|_| {
                self.number_error(
                    origin,
                    format!("Invalid floating point literal: '{literal}'"),
                )
            })
        } else {
            literal
                .parse()
                .map(|value| TokenKind::Int(value, literal.into()))
                .map_err(|e| {
                    self.number_error(origin, format!("Invalid integer literal '{literal}': {e}"))
                })
        }
    }

    fn read_decimal_digits(&mut self) {
        let start = self.byte_index;
        let len = self.input[start..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        self.advance_on_line(start + len, len);
    }

    fn read_radix_number(
        &mut self,
        origin: LineCol,
        negative: bool,
        radix: u32,
        name: &str,
        prefix: &str,
    ) -> Result<TokenKind, CompilerError> {
        let start = self.byte_index + prefix.len();
        let mut digits = String::with_capacity(16);
        let sign = if negative { "-" } else { "" };
        digits.push_str(sign);
        let mut len = 0;
        for b in self.input[start..].bytes() {
            if (b as char).is_digit(radix) {
                digits.push(b as char);
            } else if b != b'_' {
                break;
            }
            len += 1;
        }
        let end = start + len;
        self.advance_on_line(end, prefix.len() + len);
        if digits.is_empty() {
            return Err(self.number_error(
                origin,
                format!("Invalid {name} literal: no digits after {prefix}"),
            ));
        }
        let value = i64::from_str_radix(&digits, radix).map_err(|e| {
            self.number_error(origin, format!("Invalid {name} literal '{digits}': {e}"))
        })?;
        let literal = format!("{sign}{prefix}{}", &self.input[start..end]);
        Ok(TokenKind::Int(value, literal.into()))
    }

    fn number_error(&self, origin: LineCol, message: String) -> CompilerError {
        CompilerError::new_with_label(self.end_span(origin), message)
            .with_code(CompilerErrorCode::TokenizerError)
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
            ('(', _) => (OpenParen, 1),
            (')', _) => (CloseParen, 1),
            ('[', _) => (OpenSquare, 1),
            (']', _) => (CloseSquare, 1),
            ('{', _) => (OpenCurly, 1),
            ('}', _) => (CloseCurly, 1),
            ('%', _) => (Mod, 1),
            (';', _) => (Semicolon, 1),
            (',', _) => (Comma, 1),
            ('@', _) => (At, 1),
            ('$', _) => (Dollar, 1),
            ('^', _) => (BitXor, 1),
            ('~', _) => (BitNot, 1),
            _ => return None,
        };
        self.advance_on_line(self.byte_index + len, len);
        Some(kind)
    }

    pub fn next_token(&mut self) -> Result<Option<Token>, CompilerError> {
        self.skip_whitespace();
        let origin = self.start_span();
        let Some(head) = self.current else {
            return Ok(None);
        };
        let kind = match head {
            'a'..='z' | 'A'..='Z' | '_' => self.read_keyword_or_ident()?,
            '0'..='9' | '-' | '.' if self.lookahead_number(head) => self.read_number()?,
            '\r' => {
                self.advance();
                if !self.eat('\n') {
                    return Err(CompilerError::new_with_label(
                        Span::unknown(),
                        format!(
                            "Broken newlines on windows? Expecting \r\n, got: {:?}",
                            self.current
                        ),
                    ));
                }
                TokenKind::Newline
            }
            '\n' => {
                self.advance();
                TokenKind::Newline
            }
            '/' if self.peek_byte(1) == Some(b'/') => {
                self.advance_on_line(self.byte_index + 2, 2);
                TokenKind::Comment(self.read_line_rest().into())
            }
            '/' if self.peek_byte(1) == Some(b'*') => {
                let start = self.byte_index + 2;
                let Some(len) = self.input[start..].find("*/") else {
                    self.advance_to(self.input.len());
                    return Err(CompilerError::new_with_label(
                        self.end_span(origin),
                        "unterminated block comment: `/*` has no closing `*/`".to_string(),
                    ));
                };
                let end = start + len;
                self.advance_to(end + 2);
                TokenKind::Comment(self.input[start..end].into())
            }
            '"' => {
                let start = self.byte_index;
                if self.input[start..].starts_with("\"\"\"") {
                    self.read_string_literal(start)?
                } else {
                    self.advance();
                    match self.try_read_plain_string() {
                        Some(body) => TokenKind::String(body.into()),
                        None => self.read_string_literal(start)?,
                    }
                }
            }
            '\'' => self.read_string_literal(self.byte_index)?,
            '#' => {
                self.advance();
                if let Some((kind, len)) = directive(&self.input[self.byte_index..]) {
                    self.advance_on_line(self.byte_index + len, len);
                    kind
                } else {
                    TokenKind::Comment(self.read_line_rest().into())
                }
            }
            _ => match self.read_punctuation(head) {
                Some(kind) => kind,
                None => {
                    return Err(CompilerError::new_with_label(
                        Span::unknown(),
                        format!("I done a bad thing: {:?}", self.current),
                    ));
                }
            },
        };
        if !matches!(kind, TokenKind::Newline | TokenKind::Comment(_)) {
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
        let offset_range = (start.offset, self.byte_offset);

        assert!(
            offset_range.0 <= offset_range.1,
            "Offset range is invalid: {:?}",
            offset_range
        );

        Span::new(
            start.file,
            (start.line, self.line),
            (start.col, self.column),
            offset_range,
        )
    }

    pub fn empty_span(&self) -> Span {
        Span::from_linecol(self.start_span())
    }

    pub fn tokenize(&mut self) -> Result<Vec<Token>, CompilerError> {
        source_fits_spans(self.source_file, self.byte_offset, self.input.len())?;
        let mut out: Vec<Token> = Vec::with_capacity(self.input.len() / 5 + 8);
        while let Some(tok) = self.next_token()? {
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
        Ok(self
            .tokenize()?
            .into_iter()
            .map(|token| token.kind)
            .collect())
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
        let src = "\"\"\"\u{e9}\nx\u{e9}\"\"\" y \"a{b}\u{e9}\" z";
        let tokens = Tokenizer::from_code("t.rock", src).tokenize().unwrap();
        let y = &tokens[1].span;
        assert_eq!(tokens[1].kind, TokenKind::Ident("y".into()));
        assert_eq!((y.line_range().0, y.col_range().0), (1, 6));
        assert_eq!(y.offset_range().0, 13);
        let z = &tokens[3].span;
        assert_eq!(tokens[3].kind, TokenKind::Ident("z".into()));
        assert_eq!((z.line_range().0, z.col_range().0), (1, 16));
        assert_eq!(z.offset_range().0, 24);
        let err = Tokenizer::from_code("t.rock", "\"\u{e9}\\\nab\\q\"")
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

    #[test]
    fn a_non_scalar_codepoint_is_rejected() {
        assert!(rejection(r#""\u{D800}""#).contains("surrogate"));
        assert!(rejection(r#""\u{DFFF}""#).contains("surrogate"));
        assert!(rejection(r#""\u{110000}""#).contains("above"));
        assert_eq!(text(r#""\u{D7FF}""#), "\u{D7FF}");
        assert_eq!(text(r#""\u{E000}""#), "\u{E000}");
        assert_eq!(text(r#""\u{10FFFF}""#), "\u{10FFFF}");
    }

    #[test]
    fn an_escape_inside_an_interpolation_hole_still_resolves() {
        assert_eq!(text(r#""x={f("\u{41}")}""#), "x={f(\"\\u{41}\")}");
    }
}
