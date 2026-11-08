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
    Await,

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

    // Macro definition keyword (declarative macros)
    Macro,

    // Preprocessor directives
    DirectiveIf,     // #if
    DirectiveElseIf, // #elseif
    DirectiveElse,   // #else
    DirectiveEnd,    // #end

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
    IntDiv,
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
                | IntDiv
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
            DoubleDot => left(12), // Range operator: between logical ops and comparisons

            AddAssign | SubAssign | MulAssign | DivAssign => right(9),

            Or => left(13),
            And => left(15),

            As => left(37),

            Equals | NotEquals => left(19),

            LeftShift | RightShift => left(23),

            LessThan | LessThanEqual | GreaterThan | GreaterThanEqual => left(25),

            BitAnd => left(27),
            BitXor => left(28),
            BitOr => left(29),

            Plus | Minus => left(31),
            // `%` belongs with `*` and `/`, at the SAME level so the three are
            // left-associative among themselves. It sat at 21 -- under the
            // shifts and the comparisons, and under `+`/`-` -- so `a + b % c`
            // was `(a + b) % c`, `a % b * c` was `a % (b * c)`, and `a % b < c`
            // was `a % (b < c)`, which the typechecker rejected outright.
            // Pinned by `tests/operators/mod_precedence.rock`.
            Mul | Div | IntDiv | Mod => left(35),

            Dot | Colon => left(51),

            Dollar => right(1),

            // Above every assignment form (Assign 5, the compound assigns 9)
            // and below `||` (13), matching C#. At 3 it sat BELOW Assign, so
            // `x = a ?? b` parsed as `(x = a) ?? b`: the assignment's RHS is
            // parsed at min_bp 5, `??` stopped it, and the outer loop then
            // took the whole assignment as the coalesce's left operand.
            DoubleQuestionMark => left(11),

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
            TokenKind::IntDiv => "__op_idiv",
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
            TokenKind::Await => "await",
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
        if let Some(fixed) = self.fixed_spelling() {
            return fixed.to_owned();
        }
        match self {
            Self::Comment(body) => format!("//{body}"),
            Self::Ident(name) => name.to_string(),
            Self::Float(val) => val.to_string(),
            Self::Int(_, kept) => kept.to_string(),
            Self::String(body) => body.to_string(),
            Self::RawString(body) => body.to_string(),
            _ => self.fixed_spelling().unwrap_or("").to_owned(),
        }
    }

    fn fixed_spelling(&self) -> Option<&'static str> {
        Some(match self {
            Self::Dollar => "$",
            Self::At => "@",
            Self::Bang => "!",
            Self::QuestionMark => "?",
            Self::QuestionDot => "?.",
            Self::DoubleQuestionMark => "??",
            Self::TripleQuestionMark => "???",
            Self::SingleQuote => "'",
            Self::DoubleQuote => "\"",
            Self::Dot => ".",
            Self::DoubleDot => "..",
            Self::TripleDot => "...",
            Self::Colon => ":",
            Self::DoubleColon => "::",
            Self::Semicolon => ";",
            Self::Comma => ",",
            Self::OpenCurly => "{",
            Self::CloseCurly => "}",
            Self::OpenSquare => "[",
            Self::CloseSquare => "]",
            Self::OpenParen => "(",
            Self::CloseParen => ")",
            Self::LessThan => "<",
            Self::LessThanEqual => "<=",
            Self::LeftShift => "<<",
            Self::GreaterThan => ">",
            Self::GreaterThanEqual => ">=",
            Self::RightShift => ">>",
            Self::Equals => "==",
            Self::NotEquals => "!=",
            Self::Assign => "=",
            Self::FatArrow => "=>",
            Self::Arrow => "->",
            Self::AddAssign => "+=",
            Self::SubAssign => "-=",
            Self::MulAssign => "*=",
            Self::DivAssign => "/=",
            Self::Plus => "+",
            Self::PlusPlus => "++",
            Self::Minus => "-",
            Self::MinusMinus => "--",
            Self::Mul => "*",
            Self::Div => "/",
            Self::IntDiv => "./",
            Self::Mod => "%",
            Self::BitAnd => "&",
            Self::And => "and",
            Self::BitOr => "|",
            Self::Or => "or",
            Self::BitXor => "^",
            Self::BitNot => "~",
            Self::Fn => "fn",
            Self::Operator => "operator",
            Self::Table => "table",
            Self::Type => "type",
            Self::As => "as",
            Self::Yield => "yield",
            Self::Await => "await",
            Self::Let => "let",
            Self::Return => "return",
            Self::Defer => "defer",
            Self::Struct => "struct",
            Self::Newtype => "newtype",
            Self::If => "if",
            Self::Else => "else",
            Self::For => "for",
            Self::Loop => "loop",
            Self::In => "in",
            Self::While => "while",
            Self::Break => "break",
            Self::Continue => "continue",
            Self::Null => "null",
            Self::Enum => "enum",
            Self::Match => "match",
            Self::Extern => "extern",
            Self::Global => "global",
            Self::Const => "const",
            Self::Deco => "deco",
            Self::Macro => "macro",
            Self::True => "true",
            Self::False => "false",
            Self::DirectiveIf => "#if",
            Self::DirectiveElseIf => "#elseif",
            Self::DirectiveElse => "#else",
            Self::DirectiveEnd => "#end",
            Self::Newline => "\n",
            Self::EOF => "EOF",
            _ => return None,
        })
    }
}
