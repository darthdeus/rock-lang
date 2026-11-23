use crate::interp::{Flavor, Piece, scan_string};
use crate::tokenizer::directive;

/// What the source at a byte offset lexically is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexRegion {
    Code,
    Comment,
    StringText,
}

pub fn region_at(src: &str, offset: usize) -> LexRegion {
    let b = src.as_bytes();
    let mut i = 0;
    while i < src.len() && i < offset {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                i = line_end(b, i + 2);
                if offset <= i {
                    return LexRegion::Comment;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let Some(len) = src[i + 2..].find("*/") else {
                    return LexRegion::Comment;
                };
                i += 2 + len + 2;
                if offset < i {
                    return LexRegion::Comment;
                }
            }
            b'#' => {
                if let Some((_, len)) = directive(&src[i + 1..]) {
                    i += 1 + len;
                } else {
                    i = line_end(b, i + 1);
                    if offset <= i {
                        return LexRegion::Comment;
                    }
                }
            }
            b'"' | b'\'' | b'r' if literal_opens_at(src, i) => {
                let scan = scan_string(src, i);
                if offset < scan.end || (!scan.terminated && offset == scan.end) {
                    for piece in &scan.pieces {
                        let Piece::Hole {
                            range,
                            expr,
                            spec,
                            closed,
                        } = piece
                        else {
                            continue;
                        };
                        let end = range.1 - usize::from(*closed);
                        if range.0 < offset
                            && (offset <= end && (*closed || offset < end || end == src.len()))
                        {
                            let spec = spec.filter(|&(start, _)| offset >= start);
                            let part = spec.unwrap_or(*expr);
                            let classify = if spec.is_some() {
                                format_region
                            } else {
                                region_at
                            };
                            let hole = &src[part.0..part.1];
                            if scan.flavor == Flavor::Quoted && hole.contains('\\') {
                                let (cooked, offsets) = scan.hole_source(src, part);
                                let at = offsets.partition_point(|&raw| raw < offset);
                                return classify(&cooked, at);
                            }
                            return classify(hole, offset - part.0);
                        }
                    }
                    return LexRegion::StringText;
                }
                i = scan.end;
            }
            _ => i += 1,
        }
    }
    LexRegion::Code
}

fn format_region(spec: &str, offset: usize) -> LexRegion {
    let (mut depth, mut start) = (0usize, 0);
    for (at, &b) in spec.as_bytes().iter().take(offset).enumerate() {
        match b {
            b'{' => {
                if depth == 0 {
                    start = at + 1;
                }
                depth += 1;
            }
            b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if depth == 0 {
        LexRegion::StringText
    } else {
        region_at(&spec[start..], offset - start)
    }
}

fn literal_opens_at(src: &str, i: usize) -> bool {
    let b = src.as_bytes();
    if b[i] == b'r' && i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_') {
        return false;
    }
    Flavor::at(src, i).is_some()
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
