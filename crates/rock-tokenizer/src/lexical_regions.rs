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
            // `r"` opens a raw string only in identifier-start position;
            // otherwise the `r` is the tail of a longer identifier.
            b'"' | b'\'' | b'r' if literal_opens_at(src, i) => {
                i = string_regions(src, i, emit);
            }
            _ => i += 1,
        }
    }
}

fn literal_opens_at(src: &str, i: usize) -> bool {
    let b = src.as_bytes();
    if b[i] == b'r' && (i > 0 && is_ident_byte(b[i - 1])) {
        return false;
    }
    crate::interp::Flavor::at(src, i).is_some()
}

/// Emit the regions of the literal opening at `src[start]`: its text pieces as
/// `StringText`, its interpolation holes as `Code`, and (recursively) any
/// literal nested inside a hole. Returns the index just past the literal.
fn string_regions(
    src: &str,
    start: usize,
    emit: &mut dyn FnMut(usize, usize, LexRegion),
) -> usize {
    let scan = crate::interp::scan_string(src, start);
    // The whole literal is text; the holes carve `Code` back out of it, and
    // `region_at` takes the innermost match.
    let last = if scan.terminated { scan.end - 1 } else { scan.end };
    emit(start, last, LexRegion::StringText);
    for piece in &scan.pieces {
        if let crate::interp::Piece::Hole { range, closed, .. } = piece {
            // An unclosed hole that ran to end of input has no `}` to exclude.
            let hole_last =
                if !closed && range.1 == src.len() { range.1 } else { range.1 - 1 };
            emit(range.0, hole_last, LexRegion::Code);
            let mut j = range.0 + 1;
            while j < range.1 {
                if literal_opens_at(src, j) {
                    j = string_regions(src, j, emit).max(j + 1);
                } else {
                    j += 1;
                }
            }
        }
    }
    scan.end
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
