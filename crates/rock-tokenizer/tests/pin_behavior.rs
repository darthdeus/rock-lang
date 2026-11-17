use ordered_float::OrderedFloat;
use rock_tokenizer::{TokenKind, TokenKind as K, Tokenizer};

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

/// Every fixed-text kind prints as its text, and lexing that text yields the
/// kind back unless the lexer spells it another way.
#[test]
fn pin_fixed_kinds_print_and_lex_as_their_text() {
    let unlexed = [
        K::And,
        K::Or,
        K::SingleQuote,
        K::DoubleQuote,
        K::Newline,
        K::EOF,
    ];
    let cases = [
        (K::Fn, "fn"),
        (K::Operator, "operator"),
        (K::Table, "table"),
        (K::Type, "type"),
        (K::As, "as"),
        (K::Yield, "yield"),
        (K::Await, "await"),
        (K::Let, "let"),
        (K::Return, "return"),
        (K::Defer, "defer"),
        (K::Struct, "struct"),
        (K::Newtype, "newtype"),
        (K::If, "if"),
        (K::Else, "else"),
        (K::For, "for"),
        (K::Loop, "loop"),
        (K::In, "in"),
        (K::While, "while"),
        (K::Break, "break"),
        (K::Continue, "continue"),
        (K::Null, "null"),
        (K::Enum, "enum"),
        (K::Match, "match"),
        (K::Extern, "extern"),
        (K::Global, "global"),
        (K::Const, "const"),
        (K::Deco, "deco"),
        (K::Macro, "macro"),
        (K::DirectiveIf, "#if"),
        (K::DirectiveElseIf, "#elseif"),
        (K::DirectiveElse, "#else"),
        (K::DirectiveEnd, "#end"),
        (K::Arrow, "->"),
        (K::FatArrow, "=>"),
        (K::Assign, "="),
        (K::AddAssign, "+="),
        (K::SubAssign, "-="),
        (K::MulAssign, "*="),
        (K::DivAssign, "/="),
        (K::And, "and"),
        (K::Or, "or"),
        (K::Equals, "=="),
        (K::NotEquals, "!="),
        (K::OpenParen, "("),
        (K::CloseParen, ")"),
        (K::OpenSquare, "["),
        (K::CloseSquare, "]"),
        (K::OpenCurly, "{"),
        (K::CloseCurly, "}"),
        (K::LessThan, "<"),
        (K::LessThanEqual, "<="),
        (K::GreaterThan, ">"),
        (K::GreaterThanEqual, ">="),
        (K::Semicolon, ";"),
        (K::Comma, ","),
        (K::Colon, ":"),
        (K::DoubleColon, "::"),
        (K::Dot, "."),
        (K::DoubleDot, ".."),
        (K::TripleDot, "..."),
        (K::At, "@"),
        (K::Bang, "!"),
        (K::True, "true"),
        (K::False, "false"),
        (K::Plus, "+"),
        (K::PlusPlus, "++"),
        (K::Minus, "-"),
        (K::MinusMinus, "--"),
        (K::Mul, "*"),
        (K::Div, "/"),
        (K::IntDiv, "./"),
        (K::Mod, "%"),
        (K::BitAnd, "&"),
        (K::BitOr, "|"),
        (K::BitXor, "^"),
        (K::BitNot, "~"),
        (K::LeftShift, "<<"),
        (K::RightShift, ">>"),
        (K::SingleQuote, "'"),
        (K::DoubleQuote, "\""),
        (K::QuestionMark, "?"),
        (K::QuestionDot, "?."),
        (K::DoubleQuestionMark, "??"),
        (K::TripleQuestionMark, "???"),
        (K::Dollar, "$"),
        (K::Newline, "\n"),
        (K::EOF, "EOF"),
    ];
    for (kind, text) in cases {
        assert_eq!(kind.to_code(), text, "to_code for {kind:?}");
        assert_eq!(kind.to_string(), text, "Display for {kind:?}");
        if !unlexed.contains(&kind) {
            assert_eq!(kinds(text), [kind], "lex {text:?}");
        }
    }
    assert_eq!(TokenKind::Comment(" hi".into()).to_code(), "// hi");
    assert_eq!(TokenKind::Ident("foo".into()).to_code(), "foo");
    assert_eq!(TokenKind::Int(255, "0xFF".into()).to_code(), "0xFF");
    assert_eq!(TokenKind::Int(5, "0b101".into()).to_code(), "0b101");
    assert_eq!(TokenKind::Float(OrderedFloat(0.4)).to_code(), "0.4");
    assert_eq!(TokenKind::String("hello".into()).to_code(), "hello");
    assert_eq!(TokenKind::RawString("raw".into()).to_code(), "raw");
    for ident in ["foo", "_foo", "foo_bar", "EOF"] {
        assert_eq!(kinds(ident), [TokenKind::Ident(ident.into())]);
    }
    assert_eq!(kinds("&&"), [TokenKind::And]);
    assert_eq!(kinds("||"), [TokenKind::Or]);
    assert_eq!(kinds("and"), [TokenKind::Ident("__rock_and".into())]);
    assert_eq!(kinds("or"), [TokenKind::Ident("__rock_or".into())]);
    assert_eq!(kinds("do"), [TokenKind::Ident("__rock_do".into())]);
    assert_eq!(kinds("nil"), [TokenKind::Ident("__rock_nil".into())]);
    assert!(kinds_err("'").contains("unterminated"));
    assert!(kinds_err("\"").contains("unterminated"));
}

#[test]
fn pin_numbers_decimal_and_float() {
    let ints: Vec<(&str, i64, &str)> = vec![
        ("0", 0, "0"),
        ("1", 1, "1"),
        ("23", 23, "23"),
        ("456", 456, "456"),
        ("00", 0, "00"),
        ("0123", 123, "0123"),
        ("-0", 0, "-0"),
        ("-1", -1, "-1"),
        ("-7", -7, "-7"),
    ];
    for (src, value, original) in &ints {
        assert_eq!(
            kinds(src),
            vec![TokenKind::Int(*value, (*original).into())],
            "int {:?}",
            src
        );
    }
    let floats: Vec<(&str, f64)> = vec![
        ("0.4", 0.4),
        ("2.3", 2.3),
        ("2.5", 2.5),
        ("0.0", 0.0),
        (".35", 0.35),
        (".5", 0.5),
        (".0", 0.0),
        ("00.5", 0.5),
        ("-.3", -0.3),
        ("-.5", -0.5),
        ("1e3", 1000.0),
        ("1E3", 1000.0),
        ("1e0", 1.0),
        ("5e0", 5.0),
        ("1.5e3", 1500.0),
        ("2.0E-4", 0.0002),
        ("3e10", 3e10),
        ("-1.5e-3", -0.0015),
        ("1.5E-3", 0.0015),
        ("4E+5", 400000.0),
        ("1e+5", 100000.0),
        ("1e-5", 1e-5),
    ];
    for (src, value) in &floats {
        assert_eq!(
            kinds(src),
            vec![TokenKind::Float(OrderedFloat(*value))],
            "float {:?}",
            src
        );
    }
    assert_eq!(
        kinds("1 23 456 0.4"),
        vec![
            TokenKind::Int(1, "1".into()),
            TokenKind::Int(23, "23".into()),
            TokenKind::Int(456, "456".into()),
            TokenKind::Float(OrderedFloat(0.4)),
        ]
    );
    assert_eq!(
        kinds("-1 2.3 -7 -.3"),
        vec![
            TokenKind::Int(-1, "-1".into()),
            TokenKind::Float(OrderedFloat(2.3)),
            TokenKind::Int(-7, "-7".into()),
            TokenKind::Float(OrderedFloat(-0.3)),
        ]
    );
    assert_eq!(
        kinds("1_000"),
        vec![
            TokenKind::Int(1, "1".into()),
            TokenKind::Ident("_000".into())
        ],
        "decimal underscores split"
    );
    assert_eq!(
        kinds("123abc"),
        vec![
            TokenKind::Int(123, "123".into()),
            TokenKind::Ident("abc".into())
        ]
    );
    assert_eq!(
        kinds("456."),
        vec![TokenKind::Int(456, "456".into()), TokenKind::Dot]
    );
    assert_eq!(
        kinds("0."),
        vec![TokenKind::Int(0, "0".into()), TokenKind::Dot]
    );
    assert_eq!(
        kinds("2./4"),
        vec![
            TokenKind::Int(2, "2".into()),
            TokenKind::IntDiv,
            TokenKind::Int(4, "4".into()),
        ]
    );
    assert_eq!(
        kinds("5.double()"),
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
fn pin_numbers_hex_binary_underscores() {
    assert_eq!(kinds("0b101"), vec![TokenKind::Int(5, "0b101".into())]);
    assert_eq!(kinds("0B101"), vec![TokenKind::Int(5, "0b101".into())]);
    assert_eq!(kinds("0b1_0_1"), vec![TokenKind::Int(5, "0b1_0_1".into())]);
    assert_eq!(kinds("0b_101"), vec![TokenKind::Int(5, "0b_101".into())]);
    assert_eq!(kinds("0b1__0"), vec![TokenKind::Int(2, "0b1__0".into())]);
    assert_eq!(kinds("0b0"), vec![TokenKind::Int(0, "0b0".into())]);
    assert_eq!(kinds("0xFF"), vec![TokenKind::Int(255, "0xFF".into())]);
    assert_eq!(kinds("0Xff"), vec![TokenKind::Int(255, "0xff".into())]);
    assert_eq!(kinds("0x1_F"), vec![TokenKind::Int(31, "0x1_F".into())]);
    assert_eq!(kinds("0x_FF"), vec![TokenKind::Int(255, "0x_FF".into())]);
    assert_eq!(kinds("0x1__F"), vec![TokenKind::Int(31, "0x1__F".into())]);
    assert_eq!(kinds("0x0"), vec![TokenKind::Int(0, "0x0".into())]);
    assert_eq!(kinds("-0b101"), vec![TokenKind::Int(-5, "-0b101".into())]);
    assert_eq!(kinds("-0xFF"), vec![TokenKind::Int(-255, "-0xFF".into())]);
}

#[test]
fn pin_numbers_malformed_errors() {
    assert!(kinds_err("0b").contains("no digits after 0b"));
    assert!(kinds_err("0b2").contains("no digits after 0b"));
    assert!(kinds_err("0x").contains("no digits after 0x"));
    assert!(kinds_err("0xG").contains("no digits after 0x"));
    assert!(kinds_err("1e").contains("expected digits after exponent"));
    assert!(kinds_err("1E+").contains("expected digits after exponent"));
    assert!(kinds_err("1.5e").contains("expected digits after exponent"));
    assert!(kinds_err("9999999999999999999999").contains("Invalid integer literal"));
    assert!(
        kinds_err("0b1111111111111111111111111111111111111111111111111111111111111111")
            .contains("Invalid binary literal")
    );
    assert!(
        kinds_err("let mut x = 5")
            .contains("`let mut` is not supported, all bindings are mut, use just `let`")
    );
}

#[test]
fn pin_minus_and_dot_context() {
    let int = |n: i64| K::Int(n, n.to_string().as_str().into());
    let ident = |name: &str| K::Ident(name.into());
    let float = |n| K::Float(OrderedFloat(n));
    let cases: &[(&str, &[K])] = &[
        ("x = -5", &[ident("x"), K::Assign, int(-5)]),
        ("(-10)", &[K::OpenParen, int(-10), K::CloseParen]),
        (
            "[-1, -2]",
            &[K::OpenSquare, int(-1), K::Comma, int(-2), K::CloseSquare],
        ),
        ("5-3", &[int(5), K::Minus, int(3)]),
        ("x-1", &[ident("x"), K::Minus, int(1)]),
        ("true-false", &[K::True, K::Minus, K::False]),
        ("null-10", &[K::Null, K::Minus, int(10)]),
        ("--5", &[K::MinusMinus, int(5)]),
        ("- 5", &[K::Minus, int(5)]),
        (
            "restitution: .35",
            &[ident("restitution"), K::Colon, float(0.35)],
        ),
        (
            "[.5, .25]",
            &[
                K::OpenSquare,
                float(0.5),
                K::Comma,
                float(0.25),
                K::CloseSquare,
            ],
        ),
        ("2 + .5", &[int(2), K::Plus, float(0.5)]),
        ("w.0", &[ident("w"), K::Dot, int(0)]),
        ("w.0.0", &[ident("w"), K::Dot, int(0), K::Dot, int(0)]),
        ("0..5", &[int(0), K::DoubleDot, int(5)]),
        (
            "2..WIDTH-2",
            &[int(2), K::DoubleDot, ident("WIDTH"), K::Minus, int(2)],
        ),
        (
            "0..SIZE*2-1",
            &[
                int(0),
                K::DoubleDot,
                ident("SIZE"),
                K::Mul,
                int(2),
                K::Minus,
                int(1),
            ],
        ),
        ("2...3", &[int(2), K::TripleDot, int(3)]),
        ("2....3", &[int(2), K::TripleDot, float(0.3)]),
    ];
    for &(source, expected) in cases {
        assert_eq!(kinds(source), expected, "{source:?}");
    }
}

#[test]
fn pin_every_operator_and_multi_char_operator() {
    let [a, b, c, x, y] = ["a", "b", "c", "x", "y"].map(|name| K::Ident(name.into()));
    let cases: &[(&str, &[K])] = &[
        ("....", &[K::TripleDot, K::Dot]),
        (".....", &[K::TripleDot, K::DoubleDot]),
        ("......", &[K::TripleDot, K::TripleDot]),
        (":::", &[K::DoubleColon, K::Colon]),
        ("::::", &[K::DoubleColon, K::DoubleColon]),
        ("??.", &[K::DoubleQuestionMark, K::Dot]),
        ("???.", &[K::TripleQuestionMark, K::Dot]),
        ("?.?", &[K::QuestionDot, K::QuestionMark]),
        ("a+b", &[a, K::Plus, b]),
        ("a++", &[a, K::PlusPlus]),
        ("a--", &[a, K::MinusMinus]),
        ("a:b", &[a, K::Colon, b]),
        ("a::b", &[a, K::DoubleColon, b]),
        ("a;b", &[a, K::Semicolon, b]),
        ("a,b", &[a, K::Comma, b]),
        ("x?.y", &[x, K::QuestionDot, y]),
        ("x??y", &[x, K::DoubleQuestionMark, y]),
        ("x???y", &[x, K::TripleQuestionMark, y]),
        ("a ./ b", &[a, K::IntDiv, b]),
        ("a./b", &[a, K::IntDiv, b]),
        ("$x", &[K::Dollar, x]),
        ("@x", &[K::At, x]),
        ("!a", &[K::Bang, a]),
        ("a!=b", &[a, K::NotEquals, b]),
        ("a==b", &[a, K::Equals, b]),
        ("a?b:c", &[a, K::QuestionMark, b, K::Colon, c]),
    ];
    for &(source, expected) in cases {
        assert_eq!(kinds(source), expected, "{source:?}");
    }
    assert_eq!(
        kinds("fn // hello\n if /* world */ (1 + 3)"),
        vec![
            TokenKind::Fn,
            TokenKind::Comment(" hello".into()),
            TokenKind::If,
            TokenKind::Comment(" world ".into()),
            TokenKind::OpenParen,
            TokenKind::Int(1, "1".into()),
            TokenKind::Plus,
            TokenKind::Int(3, "3".into()),
            TokenKind::CloseParen,
        ]
    );
}

#[test]
fn pin_comments_and_directives() {
    assert_eq!(kinds("// line"), vec![TokenKind::Comment(" line".into())]);
    assert_eq!(kinds("//"), vec![TokenKind::Comment("".into())]);
    assert_eq!(kinds("///"), vec![TokenKind::Comment("/".into())]);
    assert_eq!(
        kinds("/* block */"),
        vec![TokenKind::Comment(" block ".into())]
    );
    assert_eq!(kinds("/**/"), vec![TokenKind::Comment("".into())]);
    assert!(kinds_err("/*/").contains("unterminated block comment"));
    assert!(kinds_err("/* unterminated").contains("unterminated block comment"));
    assert!(kinds_err("/* a *").contains("unterminated block comment"));
    assert_eq!(kinds("#"), vec![TokenKind::Comment("".into())]);
    assert_eq!(kinds("# foo"), vec![TokenKind::Comment(" foo".into())]);
    assert_eq!(
        kinds("#if x"),
        vec![TokenKind::DirectiveIf, TokenKind::Ident("x".into())]
    );
    assert_eq!(
        kinds("#foo bar"),
        vec![TokenKind::Comment("foo bar".into())]
    );
    assert_eq!(kinds("#endif"), vec![TokenKind::Comment("endif".into())]);
    assert_eq!(kinds("#ELSE"), vec![TokenKind::Comment("ELSE".into())]);
    let t = toks("fn // hi\n if");
    assert_eq!(t.len(), 3);
    assert_eq!(t[0].kind, TokenKind::Fn);
    assert_eq!(t[1].kind, TokenKind::Comment(" hi".into()));
    assert_eq!(t[2].kind, TokenKind::If);
    assert_eq!(t[0].trailing_newlines, 0);
    assert_eq!(t[1].trailing_newlines, 1);
}

#[test]
fn pin_strings_and_interpolation_boundaries() {
    assert_eq!(kinds("\"hello\""), vec![TokenKind::String("hello".into())]);
    assert_eq!(kinds("'world'"), vec![TokenKind::String("world".into())]);
    assert_eq!(kinds("\"\""), vec![TokenKind::String("".into())]);
    assert_eq!(kinds("''"), vec![TokenKind::String("".into())]);
    assert_eq!(kinds("r\"raw\""), vec![TokenKind::RawString("raw".into())]);
    assert_eq!(kinds("r\"\""), vec![TokenKind::RawString("".into())]);
    assert_eq!(
        kinds("\"\"\"tri\"\"\""),
        vec![TokenKind::String("tri".into())]
    );
    assert_eq!(
        kinds("r\"\"\"rawtri\"\"\""),
        vec![TokenKind::RawString("rawtri".into())]
    );
    assert_eq!(kinds("\"x={y}\""), vec![TokenKind::String("x={y}".into())]);
    assert_eq!(kinds("\"a{b}c\""), vec![TokenKind::String("a{b}c".into())]);
    assert_eq!(
        kinds("\"a{{b}}c\""),
        vec![TokenKind::String("a{{b}}c".into())]
    );
    assert_eq!(kinds("\"{{x}}\""), vec![TokenKind::String("{{x}}".into())]);
    assert_eq!(
        kinds("\"a{f(\"x\")}b\""),
        vec![TokenKind::String("a{f(\"x\")}b".into())]
    );
    assert_eq!(
        kinds("r\"a{b}c\""),
        vec![TokenKind::RawString("a{b}c".into())]
    );
    assert_eq!(kinds("'a{b}c'"), vec![TokenKind::String("a{b}c".into())]);
    assert_eq!(
        kinds("\"\"\"a{b}c\"\"\""),
        vec![TokenKind::String("a{b}c".into())]
    );
    assert_eq!(
        kinds("\"caf\u{e9} {x}\""),
        vec![TokenKind::String("caf\u{e9} {x}".into())]
    );
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
