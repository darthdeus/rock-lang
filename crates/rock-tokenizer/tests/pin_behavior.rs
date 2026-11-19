use rock_tokenizer::{TokenKind, Tokenizer};

fn kinds(src: &str) -> Vec<TokenKind> {
    Tokenizer::from_code("t.rock", src)
        .tokenize_kinds()
        .unwrap_or_else(|e| panic!("lex {:?} failed: {:?}", src, e.message))
}

fn kinds_err(src: &str) -> String {
    Tokenizer::from_code("t.rock", src)
        .tokenize_kinds()
        .expect_err(&format!("lex {:?} should fail", src))
        .message
}

fn toks(src: &str) -> Vec<rock_tokenizer::Token> {
    Tokenizer::from_code("t.rock", src)
        .tokenize()
        .unwrap_or_else(|e| panic!("lex {:?} failed: {:?}", src, e.message))
}

#[test]
fn pin_malformed_input_errors() {
    for (source, expected) in [
        ("/*/", "unterminated block comment"),
        ("/* unterminated", "unterminated block comment"),
        ("/* a *", "unterminated block comment"),
        ("0b", "no digits after 0b"),
        ("0b2", "no digits after 0b"),
        ("0x", "no digits after 0x"),
        ("0xG", "no digits after 0x"),
        ("1e", "expected digits after exponent"),
        ("1E+", "expected digits after exponent"),
        ("1.5e", "expected digits after exponent"),
        ("9999999999999999999999", "Invalid integer literal"),
        (
            "0b1111111111111111111111111111111111111111111111111111111111111111",
            "Invalid binary literal",
        ),
        (
            "let mut x = 5",
            "`let mut` is not supported, all bindings are mut, use just `let`",
        ),
    ] {
        let actual = kinds_err(source);
        assert!(actual.contains(expected), "{source:?}: {actual}");
    }
}

#[test]
fn pin_strings_and_interpolation_boundaries() {
    for (source, expected) in [
        ("\"hello\"", "hello"),
        ("'world'", "world"),
        ("\"\"", ""),
        ("''", ""),
        ("\"\"\"tri\"\"\"", "tri"),
        ("\"x={y}\"", "x={y}"),
        ("\"a{b}c\"", "a{b}c"),
        ("\"a{{b}}c\"", "a{{b}}c"),
        ("\"{{x}}\"", "{{x}}"),
        ("\"a{f(\"x\")}b\"", "a{f(\"x\")}b"),
        ("'a{b}c'", "a{b}c"),
        ("\"\"\"a{b}c\"\"\"", "a{b}c"),
        ("\"caf\u{e9} {x}\"", "caf\u{e9} {x}"),
    ] {
        assert_eq!(
            kinds(source),
            [TokenKind::String(expected.into())],
            "{source:?}"
        );
    }
    for (source, expected) in [
        ("r\"raw\"", "raw"),
        ("r\"\"", ""),
        ("r\"\"\"rawtri\"\"\"", "rawtri"),
        ("r\"a{b}c\"", "a{b}c"),
    ] {
        assert_eq!(
            kinds(source),
            [TokenKind::RawString(expected.into())],
            "{source:?}"
        );
    }
    assert!(kinds_err("\"unterminated").contains("unterminated"));
    assert!(kinds_err("\"unclosed {hole\"").contains("unterminated"));
}

#[test]
fn pin_spans_and_positions() {
    let t = toks("a");
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].span.line_range(), (0, 0));
    assert_eq!(t[0].span.col_range(), (0, 1));
    assert_eq!(t[0].span.offset_range(), (0, 1));
    let t = toks("a b");
    assert_eq!(t[0].span.offset_range(), (0, 1));
    assert_eq!(t[0].span.col_range(), (0, 1));
    assert_eq!(t[1].span.offset_range(), (2, 3));
    assert_eq!(t[1].span.col_range(), (2, 3));
    let t = toks("a\nb");
    assert_eq!(t[0].trailing_newlines, 1);
    assert_eq!(t[1].span.line_range(), (1, 1));
    assert_eq!(t[1].span.col_range(), (0, 1));
    assert_eq!(t[1].span.offset_range(), (2, 3));
    let t = toks("a\n\nb");
    assert_eq!(t[0].trailing_newlines, 2);
    assert_eq!(t[1].span.line_range(), (2, 2));
    let t = toks("fn\n\nfoo");
    assert_eq!(t[0].kind, TokenKind::Fn);
    assert_eq!(t[0].trailing_newlines, 2);
    assert_eq!(t[0].span.line_range(), (0, 0));
    assert_eq!(t[1].span.line_range(), (2, 2));
    let t = toks("caf\u{e9} x");
    assert_eq!(t[0].span.offset_range(), (0, 5));
    assert_eq!(t[0].span.col_range(), (0, 4));
    assert_eq!(t[1].span.offset_range(), (6, 7));
    assert_eq!(t[1].span.col_range(), (5, 6));
    let t = toks("caf\u{e9}");
    assert_eq!(t[0].kind, TokenKind::Ident("caf\u{e9}".into()));
    let t = toks("\"\"\"a\nb\"\"\"\nc");
    assert_eq!(t[0].kind, TokenKind::String("a\nb".into()));
    assert_eq!(t[0].span.line_range(), (0, 1));
    assert_eq!(t[1].kind, TokenKind::Ident("c".into()));
    assert_eq!(t[1].span.line_range().0, 2);
    let t = toks("\r\n");
    assert!(t.is_empty());
    assert!(kinds_err("\r").contains("Broken newlines"));
    let tok = Tokenizer::from_code("t.rock", "ab");
    let empty = tok.empty_span();
    assert_eq!(empty.offset_range(), (0, 0));
    assert_eq!(empty.line_range(), (0, 0));
    assert_eq!(empty.col_range(), (0, 0));
    let mut tz = Tokenizer::from_code("t.rock", "a+b");
    let first = tz.next_token().unwrap().unwrap();
    assert_eq!(first.kind, TokenKind::Ident("a".into()));
    assert_eq!(first.span.offset_range(), (0, 1));
    let second = tz.next_token().unwrap().unwrap();
    assert_eq!(second.kind, TokenKind::Plus);
    assert_eq!(second.span.offset_range(), (1, 2));
    assert!(Tokenizer::from_code("t.rock", "\u{7f}").tokenize().is_err());
}
