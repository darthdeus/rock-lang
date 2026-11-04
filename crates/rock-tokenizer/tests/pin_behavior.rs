use ordered_float::OrderedFloat;
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
fn pin_tokenkind_to_code_table() {
    let cases: Vec<(TokenKind, &str)> = vec![
        (TokenKind::Fn, "fn"),
        (TokenKind::Operator, "operator"),
        (TokenKind::Table, "table"),
        (TokenKind::Type, "type"),
        (TokenKind::As, "as"),
        (TokenKind::Yield, "yield"),
        (TokenKind::Await, "await"),
        (TokenKind::Let, "let"),
        (TokenKind::Return, "return"),
        (TokenKind::Defer, "defer"),
        (TokenKind::Struct, "struct"),
        (TokenKind::Newtype, "newtype"),
        (TokenKind::If, "if"),
        (TokenKind::Else, "else"),
        (TokenKind::For, "for"),
        (TokenKind::Loop, "loop"),
        (TokenKind::In, "in"),
        (TokenKind::While, "while"),
        (TokenKind::Break, "break"),
        (TokenKind::Continue, "continue"),
        (TokenKind::Null, "null"),
        (TokenKind::Enum, "enum"),
        (TokenKind::Match, "match"),
        (TokenKind::Extern, "extern"),
        (TokenKind::Global, "global"),
        (TokenKind::Const, "const"),
        (TokenKind::Deco, "deco"),
        (TokenKind::Macro, "macro"),
        (TokenKind::DirectiveIf, "#if"),
        (TokenKind::DirectiveElseIf, "#elseif"),
        (TokenKind::DirectiveElse, "#else"),
        (TokenKind::DirectiveEnd, "#end"),
        (TokenKind::Arrow, "->"),
        (TokenKind::FatArrow, "=>"),
        (TokenKind::Assign, "="),
        (TokenKind::AddAssign, "+="),
        (TokenKind::SubAssign, "-="),
        (TokenKind::MulAssign, "*="),
        (TokenKind::DivAssign, "/="),
        (TokenKind::And, "and"),
        (TokenKind::Or, "or"),
        (TokenKind::Equals, "=="),
        (TokenKind::NotEquals, "!="),
        (TokenKind::OpenParen, "("),
        (TokenKind::CloseParen, ")"),
        (TokenKind::OpenSquare, "["),
        (TokenKind::CloseSquare, "]"),
        (TokenKind::OpenCurly, "{"),
        (TokenKind::CloseCurly, "}"),
        (TokenKind::LessThan, "<"),
        (TokenKind::LessThanEqual, "<="),
        (TokenKind::GreaterThan, ">"),
        (TokenKind::GreaterThanEqual, ">="),
        (TokenKind::Semicolon, ";"),
        (TokenKind::Comma, ","),
        (TokenKind::Colon, ":"),
        (TokenKind::DoubleColon, "::"),
        (TokenKind::Dot, "."),
        (TokenKind::DoubleDot, ".."),
        (TokenKind::TripleDot, "..."),
        (TokenKind::At, "@"),
        (TokenKind::Bang, "!"),
        (TokenKind::True, "true"),
        (TokenKind::False, "false"),
        (TokenKind::Plus, "+"),
        (TokenKind::PlusPlus, "++"),
        (TokenKind::Minus, "-"),
        (TokenKind::MinusMinus, "--"),
        (TokenKind::Mul, "*"),
        (TokenKind::Div, "/"),
        (TokenKind::IntDiv, "./"),
        (TokenKind::Mod, "%"),
        (TokenKind::BitAnd, "&"),
        (TokenKind::BitOr, "|"),
        (TokenKind::BitXor, "^"),
        (TokenKind::BitNot, "~"),
        (TokenKind::LeftShift, "<<"),
        (TokenKind::RightShift, ">>"),
        (TokenKind::SingleQuote, "'"),
        (TokenKind::DoubleQuote, "\""),
        (TokenKind::QuestionMark, "?"),
        (TokenKind::QuestionDot, "?."),
        (TokenKind::DoubleQuestionMark, "??"),
        (TokenKind::TripleQuestionMark, "???"),
        (TokenKind::Dollar, "$"),
        (TokenKind::Newline, "\n"),
        (TokenKind::EOF, "EOF"),
    ];
    for (kind, text) in &cases {
        assert_eq!(kind.to_code(), text.to_string(), "to_code for {:?}", kind);
        assert_eq!(
            format!("{}", kind),
            text.to_string(),
            "Display for {:?}",
            kind
        );
    }
    assert_eq!(
        TokenKind::Comment(" hi".into()).to_code(),
        "// hi".to_string()
    );
    assert_eq!(TokenKind::Ident("foo".into()).to_code(), "foo".to_string());
    assert_eq!(
        TokenKind::Int(255, "0xFF".into()).to_code(),
        "0xFF".to_string()
    );
    assert_eq!(
        TokenKind::Float(OrderedFloat(0.4)).to_code(),
        "0.4".to_string()
    );
    assert_eq!(
        TokenKind::String("hello".into()).to_code(),
        "hello".to_string()
    );
    assert_eq!(
        TokenKind::RawString("raw".into()).to_code(),
        "raw".to_string()
    );
}

#[test]
fn pin_lex_roundtrip_where_it_applies() {
    let roundtrip: Vec<(&str, TokenKind)> = vec![
        ("fn", TokenKind::Fn),
        ("operator", TokenKind::Operator),
        ("table", TokenKind::Table),
        ("type", TokenKind::Type),
        ("as", TokenKind::As),
        ("yield", TokenKind::Yield),
        ("await", TokenKind::Await),
        ("let", TokenKind::Let),
        ("return", TokenKind::Return),
        ("defer", TokenKind::Defer),
        ("struct", TokenKind::Struct),
        ("newtype", TokenKind::Newtype),
        ("if", TokenKind::If),
        ("else", TokenKind::Else),
        ("for", TokenKind::For),
        ("loop", TokenKind::Loop),
        ("in", TokenKind::In),
        ("while", TokenKind::While),
        ("break", TokenKind::Break),
        ("continue", TokenKind::Continue),
        ("null", TokenKind::Null),
        ("enum", TokenKind::Enum),
        ("match", TokenKind::Match),
        ("extern", TokenKind::Extern),
        ("global", TokenKind::Global),
        ("const", TokenKind::Const),
        ("deco", TokenKind::Deco),
        ("macro", TokenKind::Macro),
        ("#if", TokenKind::DirectiveIf),
        ("#elseif", TokenKind::DirectiveElseIf),
        ("#else", TokenKind::DirectiveElse),
        ("#end", TokenKind::DirectiveEnd),
        ("->", TokenKind::Arrow),
        ("=>", TokenKind::FatArrow),
        ("=", TokenKind::Assign),
        ("+=", TokenKind::AddAssign),
        ("-=", TokenKind::SubAssign),
        ("*=", TokenKind::MulAssign),
        ("/=", TokenKind::DivAssign),
        ("==", TokenKind::Equals),
        ("!=", TokenKind::NotEquals),
        ("(", TokenKind::OpenParen),
        (")", TokenKind::CloseParen),
        ("[", TokenKind::OpenSquare),
        ("]", TokenKind::CloseSquare),
        ("{", TokenKind::OpenCurly),
        ("}", TokenKind::CloseCurly),
        ("<", TokenKind::LessThan),
        ("<=", TokenKind::LessThanEqual),
        (">", TokenKind::GreaterThan),
        (">=", TokenKind::GreaterThanEqual),
        (";", TokenKind::Semicolon),
        (",", TokenKind::Comma),
        (":", TokenKind::Colon),
        ("::", TokenKind::DoubleColon),
        (".", TokenKind::Dot),
        ("..", TokenKind::DoubleDot),
        ("...", TokenKind::TripleDot),
        ("@", TokenKind::At),
        ("!", TokenKind::Bang),
        ("true", TokenKind::True),
        ("false", TokenKind::False),
        ("+", TokenKind::Plus),
        ("++", TokenKind::PlusPlus),
        ("-", TokenKind::Minus),
        ("--", TokenKind::MinusMinus),
        ("*", TokenKind::Mul),
        ("/", TokenKind::Div),
        ("./", TokenKind::IntDiv),
        ("%", TokenKind::Mod),
        ("&", TokenKind::BitAnd),
        ("|", TokenKind::BitOr),
        ("^", TokenKind::BitXor),
        ("~", TokenKind::BitNot),
        ("<<", TokenKind::LeftShift),
        (">>", TokenKind::RightShift),
        ("?", TokenKind::QuestionMark),
        ("?.", TokenKind::QuestionDot),
        ("??", TokenKind::DoubleQuestionMark),
        ("???", TokenKind::TripleQuestionMark),
        ("$", TokenKind::Dollar),
        ("foo", TokenKind::Ident("foo".into())),
        ("_foo", TokenKind::Ident("_foo".into())),
        ("foo_bar", TokenKind::Ident("foo_bar".into())),
    ];
    for (src, expected) in &roundtrip {
        assert_eq!(&kinds(src), &vec![*expected], "roundtrip {:?}", src);
        assert_eq!(expected.to_code(), src.to_string(), "to_code {:?}", src);
    }
    assert_eq!(kinds("&&"), vec![TokenKind::And]);
    assert_eq!(kinds("||"), vec![TokenKind::Or]);
    assert_eq!(TokenKind::And.to_code(), "and".to_string());
    assert_eq!(TokenKind::Or.to_code(), "or".to_string());
    assert_eq!(kinds("and"), vec![TokenKind::Ident("__rock_and".into())]);
    assert_eq!(kinds("or"), vec![TokenKind::Ident("__rock_or".into())]);
    assert_eq!(kinds("do"), vec![TokenKind::Ident("__rock_do".into())]);
    assert_eq!(kinds("nil"), vec![TokenKind::Ident("__rock_nil".into())]);
    assert_eq!(kinds("// hi"), vec![TokenKind::Comment(" hi".into())]);
    assert_eq!(
        TokenKind::Comment(" hi".into()).to_code(),
        "// hi".to_string()
    );
    assert!(kinds_err("'").contains("unterminated"));
    assert!(kinds_err("\"").contains("unterminated"));
    assert_eq!(TokenKind::SingleQuote.to_code(), "'".to_string());
    assert_eq!(TokenKind::DoubleQuote.to_code(), "\"".to_string());
    assert_eq!(TokenKind::EOF.to_code(), "EOF".to_string());
    assert_eq!(kinds("EOF"), vec![TokenKind::Ident("EOF".into())]);
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
    assert_eq!(
        TokenKind::Int(255, "0xFF".into()).to_code(),
        "0xFF".to_string()
    );
    assert_eq!(
        TokenKind::Int(5, "0b101".into()).to_code(),
        "0b101".to_string()
    );
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
    assert!(kinds_err("let mut x = 5").contains("let mut"));
}

#[test]
fn pin_minus_and_dot_context() {
    assert_eq!(
        kinds("x = -5"),
        vec![
            TokenKind::Ident("x".into()),
            TokenKind::Assign,
            TokenKind::Int(-5, "-5".into()),
        ]
    );
    assert_eq!(
        kinds("(-10)"),
        vec![
            TokenKind::OpenParen,
            TokenKind::Int(-10, "-10".into()),
            TokenKind::CloseParen,
        ]
    );
    assert_eq!(
        kinds("[-1, -2]"),
        vec![
            TokenKind::OpenSquare,
            TokenKind::Int(-1, "-1".into()),
            TokenKind::Comma,
            TokenKind::Int(-2, "-2".into()),
            TokenKind::CloseSquare,
        ]
    );
    assert_eq!(
        kinds("5-3"),
        vec![
            TokenKind::Int(5, "5".into()),
            TokenKind::Minus,
            TokenKind::Int(3, "3".into()),
        ]
    );
    assert_eq!(
        kinds("x-1"),
        vec![
            TokenKind::Ident("x".into()),
            TokenKind::Minus,
            TokenKind::Int(1, "1".into()),
        ]
    );
    assert_eq!(
        kinds("--5"),
        vec![TokenKind::MinusMinus, TokenKind::Int(5, "5".into())]
    );
    assert_eq!(
        kinds("- 5"),
        vec![TokenKind::Minus, TokenKind::Int(5, "5".into())]
    );
    assert_eq!(kinds(".35"), vec![TokenKind::Float(OrderedFloat(0.35))]);
    assert_eq!(
        kinds("restitution: .35"),
        vec![
            TokenKind::Ident("restitution".into()),
            TokenKind::Colon,
            TokenKind::Float(OrderedFloat(0.35)),
        ]
    );
    assert_eq!(
        kinds("[.5, .25]"),
        vec![
            TokenKind::OpenSquare,
            TokenKind::Float(OrderedFloat(0.5)),
            TokenKind::Comma,
            TokenKind::Float(OrderedFloat(0.25)),
            TokenKind::CloseSquare,
        ]
    );
    assert_eq!(
        kinds("2 + .5"),
        vec![
            TokenKind::Int(2, "2".into()),
            TokenKind::Plus,
            TokenKind::Float(OrderedFloat(0.5)),
        ]
    );
    assert_eq!(kinds("-.5"), vec![TokenKind::Float(OrderedFloat(-0.5))]);
    assert_eq!(
        kinds("w.0"),
        vec![
            TokenKind::Ident("w".into()),
            TokenKind::Dot,
            TokenKind::Int(0, "0".into()),
        ]
    );
    assert_eq!(
        kinds("w.0.0"),
        vec![
            TokenKind::Ident("w".into()),
            TokenKind::Dot,
            TokenKind::Int(0, "0".into()),
            TokenKind::Dot,
            TokenKind::Int(0, "0".into()),
        ]
    );
    assert_eq!(
        kinds("0..5"),
        vec![
            TokenKind::Int(0, "0".into()),
            TokenKind::DoubleDot,
            TokenKind::Int(5, "5".into()),
        ]
    );
    assert_eq!(
        kinds("2..WIDTH-2"),
        vec![
            TokenKind::Int(2, "2".into()),
            TokenKind::DoubleDot,
            TokenKind::Ident("WIDTH".into()),
            TokenKind::Minus,
            TokenKind::Int(2, "2".into()),
        ]
    );
    assert_eq!(
        kinds("0..SIZE*2-1"),
        vec![
            TokenKind::Int(0, "0".into()),
            TokenKind::DoubleDot,
            TokenKind::Ident("SIZE".into()),
            TokenKind::Mul,
            TokenKind::Int(2, "2".into()),
            TokenKind::Minus,
            TokenKind::Int(1, "1".into()),
        ]
    );
    assert_eq!(
        kinds("2...3"),
        vec![
            TokenKind::Int(2, "2".into()),
            TokenKind::TripleDot,
            TokenKind::Int(3, "3".into()),
        ]
    );
    assert_eq!(
        kinds("2....3"),
        vec![
            TokenKind::Int(2, "2".into()),
            TokenKind::TripleDot,
            TokenKind::Float(OrderedFloat(0.3)),
        ]
    );
}

#[test]
fn pin_every_operator_and_multi_char_operator() {
    let cases: Vec<(&str, Vec<TokenKind>)> = vec![
        ("(", vec![TokenKind::OpenParen]),
        (")", vec![TokenKind::CloseParen]),
        ("[", vec![TokenKind::OpenSquare]),
        ("]", vec![TokenKind::CloseSquare]),
        ("{", vec![TokenKind::OpenCurly]),
        ("}", vec![TokenKind::CloseCurly]),
        ("->", vec![TokenKind::Arrow]),
        ("=>", vec![TokenKind::FatArrow]),
        ("=", vec![TokenKind::Assign]),
        ("+=", vec![TokenKind::AddAssign]),
        ("-=", vec![TokenKind::SubAssign]),
        ("*=", vec![TokenKind::MulAssign]),
        ("/=", vec![TokenKind::DivAssign]),
        ("==", vec![TokenKind::Equals]),
        ("!=", vec![TokenKind::NotEquals]),
        ("<", vec![TokenKind::LessThan]),
        ("<=", vec![TokenKind::LessThanEqual]),
        ("<<", vec![TokenKind::LeftShift]),
        (">", vec![TokenKind::GreaterThan]),
        (">=", vec![TokenKind::GreaterThanEqual]),
        (">>", vec![TokenKind::RightShift]),
        (";", vec![TokenKind::Semicolon]),
        (",", vec![TokenKind::Comma]),
        (":", vec![TokenKind::Colon]),
        ("::", vec![TokenKind::DoubleColon]),
        (".", vec![TokenKind::Dot]),
        ("..", vec![TokenKind::DoubleDot]),
        ("...", vec![TokenKind::TripleDot]),
        ("./", vec![TokenKind::IntDiv]),
        ("@", vec![TokenKind::At]),
        ("$", vec![TokenKind::Dollar]),
        ("!", vec![TokenKind::Bang]),
        ("+", vec![TokenKind::Plus]),
        ("++", vec![TokenKind::PlusPlus]),
        ("-", vec![TokenKind::Minus]),
        ("--", vec![TokenKind::MinusMinus]),
        ("*", vec![TokenKind::Mul]),
        ("/", vec![TokenKind::Div]),
        ("%", vec![TokenKind::Mod]),
        ("&", vec![TokenKind::BitAnd]),
        ("&&", vec![TokenKind::And]),
        ("|", vec![TokenKind::BitOr]),
        ("||", vec![TokenKind::Or]),
        ("^", vec![TokenKind::BitXor]),
        ("~", vec![TokenKind::BitNot]),
        ("?", vec![TokenKind::QuestionMark]),
        ("?.", vec![TokenKind::QuestionDot]),
        ("??", vec![TokenKind::DoubleQuestionMark]),
        ("???", vec![TokenKind::TripleQuestionMark]),
        ("....", vec![TokenKind::TripleDot, TokenKind::Dot]),
        (".....", vec![TokenKind::TripleDot, TokenKind::DoubleDot]),
        ("......", vec![TokenKind::TripleDot, TokenKind::TripleDot]),
        (":::", vec![TokenKind::DoubleColon, TokenKind::Colon]),
        ("::::", vec![TokenKind::DoubleColon, TokenKind::DoubleColon]),
        ("??.", vec![TokenKind::DoubleQuestionMark, TokenKind::Dot]),
        ("???.", vec![TokenKind::TripleQuestionMark, TokenKind::Dot]),
        ("?.?", vec![TokenKind::QuestionDot, TokenKind::QuestionMark]),
        (
            "a+b",
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Plus,
                TokenKind::Ident("b".into()),
            ],
        ),
        (
            "a++",
            vec![TokenKind::Ident("a".into()), TokenKind::PlusPlus],
        ),
        (
            "a--",
            vec![TokenKind::Ident("a".into()), TokenKind::MinusMinus],
        ),
        (
            "a:b",
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Colon,
                TokenKind::Ident("b".into()),
            ],
        ),
        (
            "a::b",
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::DoubleColon,
                TokenKind::Ident("b".into()),
            ],
        ),
        (
            "a;b",
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Semicolon,
                TokenKind::Ident("b".into()),
            ],
        ),
        (
            "a,b",
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Comma,
                TokenKind::Ident("b".into()),
            ],
        ),
        (
            "x?.y",
            vec![
                TokenKind::Ident("x".into()),
                TokenKind::QuestionDot,
                TokenKind::Ident("y".into()),
            ],
        ),
        (
            "x??y",
            vec![
                TokenKind::Ident("x".into()),
                TokenKind::DoubleQuestionMark,
                TokenKind::Ident("y".into()),
            ],
        ),
        (
            "x???y",
            vec![
                TokenKind::Ident("x".into()),
                TokenKind::TripleQuestionMark,
                TokenKind::Ident("y".into()),
            ],
        ),
        (
            "a ./ b",
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::IntDiv,
                TokenKind::Ident("b".into()),
            ],
        ),
        (
            "a./b",
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::IntDiv,
                TokenKind::Ident("b".into()),
            ],
        ),
        ("$x", vec![TokenKind::Dollar, TokenKind::Ident("x".into())]),
        ("@x", vec![TokenKind::At, TokenKind::Ident("x".into())]),
        ("!a", vec![TokenKind::Bang, TokenKind::Ident("a".into())]),
        (
            "a!=b",
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::NotEquals,
                TokenKind::Ident("b".into()),
            ],
        ),
        (
            "a==b",
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Equals,
                TokenKind::Ident("b".into()),
            ],
        ),
        (
            "a?b:c",
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::QuestionMark,
                TokenKind::Ident("b".into()),
                TokenKind::Colon,
                TokenKind::Ident("c".into()),
            ],
        ),
    ];
    for (src, expected) in &cases {
        assert_eq!(&kinds(src), expected, "operators {:?}", src);
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
    assert_eq!(kinds("/*/"), vec![TokenKind::Comment("/".into())]);
    assert_eq!(
        kinds("/* unterminated"),
        vec![TokenKind::Comment(" unterminated".into())]
    );
    assert_eq!(kinds("#"), vec![TokenKind::Comment("".into())]);
    assert_eq!(kinds("# foo"), vec![TokenKind::Comment(" foo".into())]);
    assert_eq!(kinds("#if"), vec![TokenKind::DirectiveIf]);
    assert_eq!(kinds("#elseif"), vec![TokenKind::DirectiveElseIf]);
    assert_eq!(kinds("#else"), vec![TokenKind::DirectiveElse]);
    assert_eq!(kinds("#end"), vec![TokenKind::DirectiveEnd]);
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
    assert_eq!(kinds("/"), vec![TokenKind::Div]);
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
    assert_eq!(kinds("\"a\\nb\""), vec![TokenKind::String("a\nb".into())]);
    assert_eq!(kinds("\"a\\tb\""), vec![TokenKind::String("a\tb".into())]);
    assert_eq!(kinds("\"\\\\\""), vec![TokenKind::String("\\".into())]);
    assert_eq!(kinds("\"\\u{41}\""), vec![TokenKind::String("A".into())]);
    assert_eq!(kinds("\"a\\\n  b\""), vec![TokenKind::String("ab".into())]);
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
    assert!(kinds_err("\"bad \\q\"").contains("unknown string escape"));
    assert!(kinds_err("\"\\u41\"").contains("\\u{"));
}

#[test]
fn pin_spans_and_positions() {
    let t = toks("a");
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].span.line_range, (0, 0));
    assert_eq!(t[0].span.col_range, (0, 1));
    assert_eq!(t[0].span.offset_range, (0, 1));
    let t = toks("a b");
    assert_eq!(t[0].span.offset_range, (0, 1));
    assert_eq!(t[0].span.col_range, (0, 1));
    assert_eq!(t[1].span.offset_range, (2, 3));
    assert_eq!(t[1].span.col_range, (2, 3));
    let t = toks("a\nb");
    assert_eq!(t[0].trailing_newlines, 1);
    assert_eq!(t[1].span.line_range, (1, 1));
    assert_eq!(t[1].span.col_range, (0, 1));
    assert_eq!(t[1].span.offset_range, (2, 3));
    let t = toks("a\n\nb");
    assert_eq!(t[0].trailing_newlines, 2);
    assert_eq!(t[1].span.line_range, (2, 2));
    let t = toks("fn\n\nfoo");
    assert_eq!(t[0].kind, TokenKind::Fn);
    assert_eq!(t[0].trailing_newlines, 2);
    assert_eq!(t[0].span.line_range, (0, 0));
    assert_eq!(t[1].span.line_range, (2, 2));
    let t = toks("caf\u{e9} x");
    assert_eq!(t[0].span.offset_range, (0, 5));
    assert_eq!(t[0].span.col_range, (0, 4));
    assert_eq!(t[1].span.offset_range, (6, 7));
    assert_eq!(t[1].span.col_range, (5, 6));
    let t = toks("caf\u{e9}");
    assert_eq!(t[0].kind, TokenKind::Ident("caf\u{e9}".into()));
    let t = toks("\"\"\"a\nb\"\"\"\nc");
    assert_eq!(t[0].kind, TokenKind::String("a\nb".into()));
    assert_eq!(t[0].span.line_range, (0, 1));
    assert_eq!(t[1].kind, TokenKind::Ident("c".into()));
    assert_eq!(t[1].span.line_range.0, 2);
    let t = toks("\r\n");
    assert!(t.is_empty());
    assert!(kinds_err("\r").contains("Broken newlines"));
    let tok = Tokenizer::from_code("t.rock", "ab");
    let empty = tok.empty_span();
    assert_eq!(empty.offset_range, (0, 0));
    assert_eq!(empty.line_range, (0, 0));
    assert_eq!(empty.col_range, (0, 0));
    let mut tz = Tokenizer::from_code("t.rock", "a+b");
    let first = tz.next_token().unwrap().unwrap();
    assert_eq!(first.kind, TokenKind::Ident("a".into()));
    assert_eq!(first.span.offset_range, (0, 1));
    let second = tz.next_token().unwrap().unwrap();
    assert_eq!(second.kind, TokenKind::Plus);
    assert_eq!(second.span.offset_range, (1, 2));
    assert!(Tokenizer::from_code("t.rock", "\u{7f}").tokenize().is_err());
}
