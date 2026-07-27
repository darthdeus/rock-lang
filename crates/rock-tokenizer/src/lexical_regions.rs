//! Which lexical region a cursor sits in: code, comment, or string-literal
//! text. The tokenizer proper answers this only for source that lexes cleanly
//! and only per-token; an editor buffer is unterminated between keystrokes and
//! an interpolated string is a mix of text and code within one token. This scan
//! is error-tolerant (an unclosed literal or comment runs to end of input) and
//! splits interpolation holes out of the string they sit in.
//!
//! `region_agrees_with_tokenizer` (in this file's tests) pins the two against
//! each other over the repo's own sources.

/// What the source at a byte offset lexically is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexRegion {
    Code,
    Comment,
    /// The literal text of a string. An interpolation hole reports `Code`.
    StringText,
}

/// The region a cursor at byte `offset` sits in. A cursor is inside a region
/// from just after its opening delimiter through its closing delimiter, so
/// `"abc|"` is text and `"abc"|` is code.
pub fn region_at(src: &str, offset: usize) -> LexRegion {
    // Regions nest (a literal inside an interpolation hole inside a string), so
    // the innermost -- the latest-starting -- container wins.
    let mut found = (LexRegion::Code, 0usize, false);
    scan(src, offset, &mut |start, end, region| {
        if start < offset && offset <= end && (!found.2 || start >= found.1) {
            found = (region, start, true);
        }
    });
    found.0
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Forward scan emitting every non-code region as `(start, end, region)`, where
/// `start` is the opening delimiter's first byte and `end` is the closing
/// delimiter's LAST byte (end of input when unterminated), so a cursor anywhere
/// up to and including the closer is inside. Stops once past `limit`.
fn scan(src: &str, limit: usize, emit: &mut dyn FnMut(usize, usize, LexRegion)) {
    let b = src.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i < n && i <= limit {
        match b[i] {
            b'/' if i + 1 < n && b[i + 1] == b'/' => {
                let end = line_end(b, i + 2);
                emit(i, end, LexRegion::Comment);
                i = end;
            }
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                let close = find(b, i + 2, b"*/");
                emit(i, close.map_or(n, |c| c + 1), LexRegion::Comment);
                i = close.map_or(n, |c| c + 2);
            }
            b'#' => {
                let mut j = i + 1;
                while j < n && b[j].is_ascii_alphabetic() {
                    j += 1;
                }
                // `#if`/`#elseif`/`#else`/`#end` are conditional-compilation
                // directives; every other `#` opens a comment.
                let word = &src[i + 1..j];
                if b.get(i + 1).is_some_and(u8::is_ascii_lowercase)
                    && matches!(word, "if" | "elseif" | "else" | "end")
                {
                    i = j;
                } else {
                    let end = line_end(b, i + 1);
                    emit(i, end, LexRegion::Comment);
                    i = end;
                }
            }
            b'\'' => {
                let end = memchr(b, i + 1, b'\'').unwrap_or(n);
                emit(i, end, LexRegion::StringText);
                i = (end + 1).min(n);
            }
            // `r"` opens a raw string only in identifier-start position;
            // otherwise the `r` is the tail of a longer identifier.
            b'r' if i + 1 < n
                && b[i + 1] == b'"'
                && (i == 0 || !is_ident_byte(b[i - 1])) =>
            {
                i = raw_string(b, i, emit);
            }
            b'"' => {
                i = string(src, i, emit);
            }
            _ => i += 1,
        }
    }
}

/// A raw string (`r"..."` / `r"""..."""`): no escapes, no interpolation.
fn raw_string(b: &[u8], start: usize, emit: &mut dyn FnMut(usize, usize, LexRegion)) -> usize {
    let n = b.len();
    let (body, close) = if b[start + 1..].starts_with(b"\"\"\"") {
        (start + 4, &b"\"\"\""[..])
    } else {
        (start + 2, &b"\""[..])
    };
    match find(b, body, close) {
        Some(c) => {
            emit(start, c + close.len() - 1, LexRegion::StringText);
            c + close.len()
        }
        None => {
            emit(start, n, LexRegion::StringText);
            n
        }
    }
}

/// A `"` or `"""` string: interpolation holes are emitted as `Code`, the rest as
/// `StringText`. Escapes are honored in the `"` form only, matching the
/// tokenizer's readers.
fn string(src: &str, start: usize, emit: &mut dyn FnMut(usize, usize, LexRegion)) -> usize {
    let b = src.as_bytes();
    let n = b.len();
    let triple = b[start..].starts_with(b"\"\"\"");
    let close: &[u8] = if triple { b"\"\"\"" } else { b"\"" };
    let mut i = start + close.len();
    // Start of the string-text run that the next hole (or the closer) ends.
    let mut text_start = start;
    let mut depth: usize = 0;
    let mut hole_open = 0usize;
    while i < n {
        if depth == 0 && b[i..].starts_with(close) {
            emit(text_start, i + close.len() - 1, LexRegion::StringText);
            return i + close.len();
        }
        if !triple && b[i] == b'\\' {
            i += 2;
            continue;
        }
        if depth == 0 && (b[i] == b'{' || b[i] == b'}') && b.get(i + 1) == Some(&b[i]) {
            // `{{` / `}}` emit a literal brace and never open a hole.
            i += 2;
            continue;
        }
        if b[i] == b'{' {
            if depth == 0 {
                emit(text_start, i, LexRegion::StringText);
                hole_open = i;
            }
            depth += 1;
            i += 1;
            continue;
        }
        if b[i] == b'}' && depth > 0 {
            depth -= 1;
            if depth == 0 {
                emit(hole_open, i, LexRegion::Code);
                text_start = i;
            }
            i += 1;
            continue;
        }
        // A nested literal inside a hole is opaque -- its quotes, braces and
        // colons belong to it, not to the enclosing string.
        if depth > 0 && b[i] == b'"' {
            let inner_end = string(src, i, emit);
            i = inner_end.max(i + 1);
            continue;
        }
        i += 1;
    }
    // Unterminated: whatever was open runs to end of input.
    if depth > 0 {
        emit(hole_open, n, LexRegion::Code);
    } else {
        emit(text_start, n, LexRegion::StringText);
    }
    n
}

fn line_end(b: &[u8], from: usize) -> usize {
    memchr(b, from, b'\n').unwrap_or(b.len())
}

fn memchr(b: &[u8], from: usize, needle: u8) -> Option<usize> {
    b.get(from..)?.iter().position(|&c| c == needle).map(|p| p + from)
}

fn find(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let hay = b.get(from..)?;
    hay.windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `at(src, "marker")` = the region just after the first `|` in `src`, which
    /// is stripped before scanning.
    fn at(src: &str) -> LexRegion {
        let off = src.find('|').expect("marker");
        region_at(&src.replace('|', ""), off)
    }

    #[test]
    fn code_is_code() {
        assert_eq!(at("let x = fo|o"), LexRegion::Code);
        assert_eq!(at("|let x = 1"), LexRegion::Code);
    }

    #[test]
    fn comments() {
        assert_eq!(at("let x = 1 // no|te"), LexRegion::Comment);
        assert_eq!(at("/* no|te */ x"), LexRegion::Comment);
        assert_eq!(at("/* note */| x"), LexRegion::Code);
        assert_eq!(at("# no|te"), LexRegion::Comment);
        assert_eq!(at("#if de|bug"), LexRegion::Code);
    }

    #[test]
    fn string_text_and_holes() {
        assert_eq!(at("\"pl|ain\""), LexRegion::StringText);
        assert_eq!(at("\"plain\"|"), LexRegion::Code);
        assert_eq!(at("\"a {x.y|} b\""), LexRegion::Code);
        assert_eq!(at("\"a {x.y} b|\""), LexRegion::StringText);
        assert_eq!(at("\"{{no|t}}\""), LexRegion::StringText);
        assert_eq!(at("\"a\" + \"b|\""), LexRegion::StringText);
    }

    #[test]
    fn escapes_do_not_end_the_string() {
        assert_eq!(at("\"a\\\" st|ill\""), LexRegion::StringText);
        assert_eq!(at("\"a\\n b|\""), LexRegion::StringText);
    }

    #[test]
    fn raw_strings_never_interpolate() {
        assert_eq!(at("r\"a {x|} b\""), LexRegion::StringText);
        assert_eq!(at("r\"\"\"a {x|} b\"\"\""), LexRegion::StringText);
        // `r` in identifier-tail position is not a raw-string prefix.
        assert_eq!(at("var\"te|xt\""), LexRegion::StringText);
    }

    #[test]
    fn triple_quoted_interpolates_but_does_not_escape() {
        assert_eq!(at("\"\"\"a {x|} b\"\"\""), LexRegion::Code);
        assert_eq!(at("\"\"\"a\\ b|\"\"\""), LexRegion::StringText);
    }

    #[test]
    fn nested_literal_in_a_hole_is_text() {
        assert_eq!(at("\"{join(v, \"in|ner\")}\""), LexRegion::StringText);
        assert_eq!(at("\"{join(v, \"inner\")|}\""), LexRegion::Code);
    }

    #[test]
    fn unterminated_runs_to_end_of_input() {
        assert_eq!(at("print(\"some te|xt"), LexRegion::StringText);
        assert_eq!(at("print(\"x={he|"), LexRegion::Code);
        assert_eq!(at("/* op|en"), LexRegion::Comment);
        assert_eq!(at("// op|en"), LexRegion::Comment);
    }

    /// The scan and the real tokenizer must agree on every source that lexes:
    /// each `Comment` / string token's interior classifies as its region, and
    /// every identifier/keyword token classifies as code.
    #[test]
    fn region_agrees_with_tokenizer() {
        let corpus = [
            "fn f() { let x = 1 // note\n let y = \"a{x}b\" }",
            "// lead\nfn g() { print(\"hp={x.y} left\") }",
            "fn h() { let r = r\"raw {x}\" \n let t = \"\"\"tri {x} ple\"\"\" }",
            "fn i() { /* block\n spans */ let s = \"esc \\\" more\" }",
            "#if debug\nfn j() { print('c') }\n#end",
        ];
        for src in corpus {
            let toks = crate::Tokenizer::new(src, ustr::Ustr::from("t.rock"))
                .tokenize()
                .unwrap_or_else(|e| panic!("corpus must lex: {src:?}: {e:?}"));
            for tok in &toks {
                let (s, e) = tok.span.offset_range;
                let want = match tok.kind {
                    crate::TokenKind::Comment(_) => LexRegion::Comment,
                    crate::TokenKind::RawString(_) => LexRegion::StringText,
                    // A `String` token mixes text and holes -- the split is what
                    // the scan exists for, so the tokenizer can't adjudicate it.
                    crate::TokenKind::String(_) => continue,
                    crate::TokenKind::Newline => continue,
                    _ => LexRegion::Code,
                };
                // `e` is the token's exclusive end -- a cursor there sits after
                // the closing delimiter, which is code by this scan's rule.
                for off in s + 1..e {
                    assert_eq!(
                        region_at(src, off),
                        want,
                        "offset {off} of {src:?} (token {:?} spanning {s}..{e})",
                        tok.kind
                    );
                }
            }
        }
    }
}
