use crate::tokenizer::directive;

/// What the source at a byte offset lexically is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexRegion {
    Code,
    Comment,
    StringText,
}

pub fn region_at(src: &str, offset: usize) -> LexRegion {
    // Regions nest (a literal in a hole in a string): the innermost, latest-starting one wins.
    let (mut found, mut latest) = (LexRegion::Code, 0);
    scan(src, offset, &mut |start, end, region| {
        if start < offset && offset <= end && start >= latest {
            found = region;
            latest = start;
        }
    });
    found
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Emits `(start, end, region)` with `end` the closing delimiter's LAST byte (end of input
/// when unterminated), so a cursor up to and including the closer is inside.
fn scan(src: &str, limit: usize, emit: &mut dyn FnMut(usize, usize, LexRegion)) {
    let b = src.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i < n && i <= limit {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                let end = line_end(b, i + 2);
                emit(i, end, LexRegion::Comment);
                i = end;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                if let Some(len) = src[i + 2..].find("*/") {
                    let close = i + 2 + len;
                    emit(i, close + 1, LexRegion::Comment);
                    i = close + 2;
                } else {
                    emit(i, n, LexRegion::Comment);
                    i = n;
                }
            }
            b'#' => {
                if let Some((_, len)) = directive(&src[i + 1..]) {
                    i += 1 + len;
                } else {
                    let end = line_end(b, i + 1);
                    emit(i, end, LexRegion::Comment);
                    i = end;
                }
            }
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

fn string_regions(src: &str, start: usize, emit: &mut dyn FnMut(usize, usize, LexRegion)) -> usize {
    let scan = crate::interp::scan_string(src, start);
    let last = if scan.terminated {
        scan.end - 1
    } else {
        scan.end
    };
    emit(start, last, LexRegion::StringText);
    for piece in &scan.pieces {
        if let crate::interp::Piece::Hole { range, closed, .. } = piece {
            let hole_last = if !closed && range.1 == src.len() {
                range.1
            } else {
                range.1 - 1
            };
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
    b[from..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |len| from + len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(src: &str) -> LexRegion {
        let off = src.find('|').expect("marker");
        region_at(&src.replace('|', ""), off)
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
    fn raw_strings_never_interpolate() {
        assert_eq!(at("r\"a {x|} b\""), LexRegion::StringText);
        assert_eq!(at("r\"\"\"a {x|} b\"\"\""), LexRegion::StringText);
        assert_eq!(at("var\"te|xt\""), LexRegion::StringText);
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
                let (s, e) = tok.span.offset_range();
                let want = match tok.kind {
                    crate::TokenKind::Comment(_) => LexRegion::Comment,
                    crate::TokenKind::RawString(_) => LexRegion::StringText,
                    crate::TokenKind::String(_) => continue,
                    crate::TokenKind::Newline => continue,
                    _ => LexRegion::Code,
                };
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
