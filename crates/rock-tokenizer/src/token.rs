use crate::prelude::*;

// All variants hold only Copy payloads (Ustr / i64 / OrderedFloat<f64>), so
// TokenKind is Copy: clones become memcpys and Vec<Token>::clone bulk-copies.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Fn,
    Operator,
    Table,
    Let,
    Return,
    Defer,

    Yield,

    Struct,
    Newtype,

    If,
    Else,
    For,
    Loop,
    In,
    While,
    Break,
    Continue,
    Null,
    Enum,
    Match,
    Extern,
    Global,
    Const,
    Type,
    As,

    // Decorator definition keyword
    // Note: "wrap" and "init" are contextual keywords, recognized only in decorator context
    Deco,

    // Macro definition keyword (declarative macros, see docs/macro-system-plan.md)
    Macro,

    // Preprocessor directives
    DirectiveIf,      // #if
    DirectiveElseIf,  // #elseif
    DirectiveElse,    // #else
    DirectiveEnd,     // #end

    Arrow,
    FatArrow,

    Assign,

    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,

    And,
    Or,

    Equals,
    NotEquals,

    OpenParen,
    CloseParen,

    OpenSquare,
    CloseSquare,

    OpenCurly,
    CloseCurly,

    LessThan,
    LessThanEqual,

    GreaterThan,
    GreaterThanEqual,

    Semicolon,
    Comma,

    Colon,
    DoubleColon,

    Dot,
    DoubleDot,
    TripleDot,

    At,
    Bang,

    True,
    False,

    Plus,
    PlusPlus,

    Minus,
    MinusMinus,

    Mul,
    Div,
    Mod,

    BitAnd,
    BitOr,
    BitXor,
    BitNot,
    LeftShift,
    RightShift,

    SingleQuote,
    DoubleQuote,

    QuestionMark,
    QuestionDot,
    DoubleQuestionMark,
    TripleQuestionMark,

    Dollar,

    Comment(Ustr),
    Ident(Ustr),
    Int(i64, Ustr), // value, original_text
    Float(OrderedFloat<f64>),
    String(Ustr),
    RawString(Ustr), // Raw string literal - no escape sequences or interpolation

    Newline,
    EOF,
}

impl std::fmt::Display for TokenKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_code())
    }
}

impl TokenKind {
    pub fn is_op(&self) -> bool {
        use TokenKind::*;

        matches!(
            self,
            Dot | DoubleDot
                | TripleDot
                | Colon
                | DoubleColon
                | As
                | Bang
                | Plus
                | PlusPlus
                | Minus
                | MinusMinus
                | Mul
                | Div
                | Mod
                | And
                | Or
                | BitAnd
                | BitOr
                | BitXor
                | BitNot
                | LeftShift
                | RightShift
                | OpenSquare
                | CloseSquare
                | OpenParen
                | CloseParen
                | Assign
                | AddAssign
                | SubAssign
                | MulAssign
                | DivAssign
                | Equals
                | NotEquals
                | LessThan
                | LessThanEqual
                | GreaterThan
                | GreaterThanEqual
                | QuestionMark
                | QuestionDot
                | DoubleQuestionMark
                | TripleQuestionMark
                | Dollar
                | At
        )
    }

    pub fn is_comment(&self) -> bool {
        matches!(self, TokenKind::Comment(_))
    }

    pub fn postfix_bp(&self) -> Option<(u8, ())> {
        use TokenKind::*;
        let val = match self {
            OpenParen | OpenSquare => Some(49),
            QuestionDot => Some(51),
            At => Some(10), // @ operator for block/lambda passing (lower precedence than no-parens)
            QuestionMark | PlusPlus | MinusMinus => Some(17),
            _ => None,
        };

        Some((val?, ()))
    }

    pub fn prefix_bp(&self) -> Option<((), u8)> {
        use TokenKind::*;
        let val = match self {
            Minus => Some(41),
            PlusPlus | MinusMinus => Some(41),
            Bang => Some(41),
            BitNot => Some(41),
            // `^name` anaphoric ident: prefix `^` is otherwise illegal,
            // so reusing the BitXor token doesn't conflict with the
            // existing infix `^` (xor).
            BitXor => Some(41),
            _ => None,
        };

        Some(((), val?))
    }

    pub fn infix_bp(&self) -> Option<(u8, u8)> {
        let left = |x| Some((x, x + 1));
        let right = |x| Some((x + 1, x));

        use TokenKind::*;
        match self {
            Assign => right(5),

            DoubleColon => left(7),
            DoubleDot => left(12),  // Range operator: between logical ops and comparisons

            AddAssign | SubAssign | MulAssign | DivAssign => right(9),

            Or => left(13),
            And => left(15),

            As => left(37),

            Equals | NotEquals => left(19),

            Mod => left(21),

            LeftShift | RightShift => left(23),

            LessThan | LessThanEqual | GreaterThan | GreaterThanEqual => left(25),

            BitAnd => left(27),
            BitXor => left(28),
            BitOr => left(29),

            Plus | Minus => left(31),
            Mul | Div => left(35),

            Dot | Colon => left(51),

            Dollar => right(1),
            DoubleQuestionMark => left(3),

            _ => None,
        }
    }

    /// The overloadable binary operators (`+ - * / %`) and the base
    /// name of the operator function `a OP b` desugars to. This is the
    /// single source of truth for the operator-as-function mapping:
    /// the parser emits a call to this name, and resolution / mangling
    /// / codegen recognize it. `None` for every non-overloadable token
    /// (`== < && ?? []` etc. stay dedicated nodes). The base name (no
    /// type mangling) is what overload resolution keys on, exactly
    /// like any other overloaded function.
    pub fn overloadable_binop_fn(&self) -> Option<&'static str> {
        Some(match self {
            TokenKind::Plus => "__op_add",
            TokenKind::Minus => "__op_sub",
            TokenKind::Mul => "__op_mul",
            TokenKind::Div => "__op_div",
            TokenKind::Mod => "__op_mod",
            _ => return None,
        })
    }

    /// If this token kind is a word-shaped keyword (i.e. its `to_code()`
    /// is a Rock keyword like `match`, `if`, `for`), return that text.
    /// Used by the parser to accept keyword-named members after `.` —
    /// `s.match(p)`, `m.in(k)`, etc. don't conflict with statement-start
    /// keyword parsing because the dot resolves the ambiguity.
    ///
    /// Returns `None` for operator-shaped tokens (`+`, `==`, `->`, etc.)
    /// and for non-keyword tokens.
    pub fn as_keyword_identifier(&self) -> Option<&'static str> {
        Some(match self {
            TokenKind::Fn => "fn",
            TokenKind::Operator => "operator",
            TokenKind::Table => "table",
            TokenKind::Type => "type",
            TokenKind::As => "as",
            TokenKind::Yield => "yield",
            TokenKind::Let => "let",
            TokenKind::Return => "return",
            TokenKind::Defer => "defer",
            TokenKind::Struct => "struct",
            TokenKind::Newtype => "newtype",
            TokenKind::If => "if",
            TokenKind::Else => "else",
            TokenKind::For => "for",
            TokenKind::In => "in",
            TokenKind::Loop => "loop",
            TokenKind::While => "while",
            TokenKind::Break => "break",
            TokenKind::Continue => "continue",
            TokenKind::Null => "null",
            TokenKind::Enum => "enum",
            TokenKind::Match => "match",
            TokenKind::Extern => "extern",
            TokenKind::Global => "global",
            TokenKind::Const => "const",
            TokenKind::Deco => "deco",
            TokenKind::Macro => "macro",
            TokenKind::And => "and",
            TokenKind::Or => "or",
            TokenKind::True => "true",
            TokenKind::False => "false",
            _ => return None,
        })
    }

    pub fn to_code(&self) -> String {
        if let Some(text) = self.fixed_spelling() { return text.to_owned(); }
        match self {



























            TokenKind::Comment(ustr) => format!("//{}", ustr),
            TokenKind::Ident(ustr) => ustr.to_string(),
            TokenKind::Float(num) => num.to_string(),
            TokenKind::Int(_, original_text) => original_text.to_string(),
            TokenKind::String(ustr) => ustr.to_string(),
            TokenKind::RawString(ustr) => ustr.to_string(),
            _ => unreachable!("fixed token spelling handled above"),
        }
    }

    fn fixed_spelling(&self) -> Option<&'static str> {
        Some(match self {
            TokenKind::Fn => "fn",
            TokenKind::Operator => "operator",
            TokenKind::Table => "table",
            TokenKind::Type => "type",
            TokenKind::As => "as",
            TokenKind::DirectiveIf => "#if",
            TokenKind::DirectiveElseIf => "#elseif",
            TokenKind::DirectiveElse => "#else",
            TokenKind::DirectiveEnd => "#end",
            TokenKind::Yield => "yield",
            TokenKind::Let => "let",
            TokenKind::Return => "return",
            TokenKind::Defer => "defer",
            TokenKind::Struct => "struct",
            TokenKind::Newtype => "newtype",
            TokenKind::If => "if",
            TokenKind::Else => "else",
            TokenKind::For => "for",
            TokenKind::In => "in",
            TokenKind::Loop => "loop",
            TokenKind::While => "while",
            TokenKind::Break => "break",
            TokenKind::Continue => "continue",
            TokenKind::Null => "null",
            TokenKind::Enum => "enum",
            TokenKind::Match => "match",
            TokenKind::Extern => "extern",
            TokenKind::Global => "global",
            TokenKind::Const => "const",
            TokenKind::Deco => "deco",
            TokenKind::Macro => "macro",
            TokenKind::Arrow => "->",
            TokenKind::FatArrow => "=>",
            TokenKind::Assign => "=",
            TokenKind::AddAssign => "+=",
            TokenKind::SubAssign => "-=",
            TokenKind::MulAssign => "*=",
            TokenKind::DivAssign => "/=",
            TokenKind::And => "and",
            TokenKind::Or => "or",
            TokenKind::Equals => "==",
            TokenKind::NotEquals => "!=",
            TokenKind::OpenParen => "(",
            TokenKind::CloseParen => ")",
            TokenKind::OpenSquare => "[",
            TokenKind::CloseSquare => "]",
            TokenKind::OpenCurly => "{",
            TokenKind::CloseCurly => "}",
            TokenKind::LessThan => "<",
            TokenKind::LessThanEqual => "<=",
            TokenKind::GreaterThan => ">",
            TokenKind::GreaterThanEqual => ">=",
            TokenKind::Semicolon => ";",
            TokenKind::Comma => ",",
            TokenKind::Colon => ":",
            TokenKind::DoubleColon => "::",
            TokenKind::Dot => ".",
            TokenKind::DoubleDot => "..",
            TokenKind::TripleDot => "...",
            TokenKind::At => "@",
            TokenKind::Bang => "!",
            TokenKind::True => "true",
            TokenKind::False => "false",
            TokenKind::Plus => "+",
            TokenKind::PlusPlus => "++",
            TokenKind::Minus => "-",
            TokenKind::MinusMinus => "--",
            TokenKind::Mul => "*",
            TokenKind::Div => "/",
            TokenKind::Mod => "%",
            TokenKind::BitAnd => "&",
            TokenKind::BitOr => "|",
            TokenKind::BitXor => "^",
            TokenKind::BitNot => "~",
            TokenKind::LeftShift => "<<",
            TokenKind::RightShift => ">>",
            TokenKind::SingleQuote => "'",
            TokenKind::DoubleQuote => "\"",
            TokenKind::QuestionMark => "?",
            TokenKind::QuestionDot => "?.",
            TokenKind::DoubleQuestionMark => "??",
            TokenKind::TripleQuestionMark => "???",
            TokenKind::Dollar => "$",
            TokenKind::Newline => "\n",
            TokenKind::EOF => "EOF",
            _ => return None,
        })
    }
}

/// All Rock reserved keywords, as written in source. The
/// authoritative `name -> TokenKind` mapping lives in
/// `read_keyword_or_ident` (this crate's `tokenizer`); this list is the
/// downstream-visible mirror used by tooling (LSP completion in
/// particular). If you add a keyword to the tokenizer, append it
/// here too — a keyword round-trip test fails
/// when an entry here is NOT recognized as a keyword token, catching
/// drift in one direction. The reverse (new tokenizer keyword
/// missing from this list) is a doc-comment invariant.
///
/// Contextual keywords ("co" before `fn`, "value" before a type name,
/// "wrap"/"init" in decorator definitions, "inline") are intentionally
/// excluded — they're identifiers outside their specific syntactic
/// context, recognized by parser lookahead rather than the lexer.
pub const KEYWORDS: &[&str] = &[
    "fn", "operator", "table", "type", "as", "yield",
    "if", "else", "for", "in", "while", "loop", "break", "continue",
    "let", "return", "defer",
    "struct", "newtype",
    "null", "enum", "match", "extern", "global",
    "deco", "macro",
    "true", "false",
];
