use crate::prelude::*;

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

    Deco,

    Macro,

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

macro_rules! rock_keywords {
    ($($kind:ident = $text:literal),* $(,)?) => {
        impl TokenKind {
            pub(crate) fn keyword(text: &str) -> Option<Self> {
                Some(match text {
                    $($text => Self::$kind,)*
                    _ => return None,
                })
            }

            /// The name allowed after a member dot, including logical operators.
            pub fn as_keyword_identifier(&self) -> Option<&'static str> {
                Some(match self {
                    $(Self::$kind => $text,)*
                    Self::And => "and",
                    Self::Or => "or",
                    _ => return None,
                })
            }
        }
    };
}

rock_keywords! {
    Fn = "fn",
    Operator = "operator",
    Table = "table",
    Type = "type",
    As = "as",
    Yield = "yield",
    Await = "await",
    If = "if",
    Let = "let",
    Return = "return",
    Defer = "defer",
    Struct = "struct",
    Newtype = "newtype",
    Else = "else",
    For = "for",
    In = "in",
    While = "while",
    Loop = "loop",
    Break = "break",
    Continue = "continue",
    Null = "null",
    Enum = "enum",
    Match = "match",
    Extern = "extern",
    Global = "global",
    Const = "const",
    Deco = "deco",
    Macro = "macro",
    True = "true",
    False = "false",
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
            Minus | PlusPlus | MinusMinus | Bang | BitNot | BitXor => Some(41),
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
            Mul | Div | IntDiv | Mod => left(35),

            Dot | Colon => left(51),

            Dollar => right(1),

            // Above every assignment so `x = a ?? b` is `x = (a ?? b)`; below `||`.
            DoubleQuestionMark => left(11),

            _ => None,
        }
    }

    /// Single source of truth for the operator-as-function mapping the parser emits and
    /// resolution, mangling and codegen recognize.
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

    pub fn operator_spelling(&self) -> &'static str {
        use TokenKind::*;
        match self {
            And => "and",
            Or => "or",
            Equals => "==",
            NotEquals => "!=",
            LessThan => "<",
            LessThanEqual => "<=",
            GreaterThan => ">",
            GreaterThanEqual => ">=",
            LeftShift => "<<",
            RightShift => ">>",
            BitAnd => "&",
            BitOr => "|",
            BitXor => "^",
            Plus => "+",
            Minus => "-",
            Mul => "*",
            Div => "/",
            IntDiv => "./",
            Mod => "%",
            AddAssign => "+=",
            SubAssign => "-=",
            MulAssign => "*=",
            DivAssign => "/=",
            DoubleDot => "..",
            DoubleQuestionMark => "??",
            DoubleColon => "::",
            As => "as",
            Dot => ".",
            Colon => ":",
            Bang => "!",
            BitNot => "~",
            OpenSquare => "[",
            _ => unreachable!("operator token"),
        }
    }
}
