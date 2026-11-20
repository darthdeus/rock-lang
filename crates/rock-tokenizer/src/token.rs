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

#[derive(Clone, Copy)]
pub enum BindingPower {
    Postfix(u8),
    Infix(u8, u8),
}

macro_rules! operators {
    (@bp left $bp:literal) => { Some(BindingPower::Infix($bp, $bp + 1)) };
    (@bp right $bp:literal) => { Some(BindingPower::Infix($bp + 1, $bp)) };
    ($($kind:ident $($text:literal)? $(= $assoc:ident($bp:literal))? $(=> $func:literal)? ,)*) => {
        impl TokenKind {
            pub fn is_op(&self) -> bool {
                matches!(
                    self,
                    $(Self::$kind |)*
                    Self::TripleDot | Self::PlusPlus | Self::MinusMinus
                        | Self::CloseSquare | Self::OpenParen | Self::CloseParen
                        | Self::QuestionMark | Self::QuestionDot | Self::TripleQuestionMark
                        | Self::At
                )
            }

            #[inline]
            pub fn binding_power(&self) -> Option<BindingPower> {
                match self {
                    $($(Self::$kind => operators!(@bp $assoc $bp),)?)*
                    Self::OpenParen | Self::OpenSquare => Some(BindingPower::Postfix(49)),
                    Self::QuestionDot => Some(BindingPower::Postfix(51)),
                    Self::At => Some(BindingPower::Postfix(10)),
                    Self::QuestionMark | Self::PlusPlus | Self::MinusMinus => {
                        Some(BindingPower::Postfix(17))
                    }
                    _ => None,
                }
            }

            pub fn operator_spelling(&self) -> &'static str {
                match self {
                    $($(Self::$kind => $text,)?)*
                    _ => unreachable!("operator token"),
                }
            }

            pub fn overloadable_binop_fn(&self) -> Option<&'static str> {
                match self {
                    $($(Self::$kind => Some($func),)?)*
                    _ => None,
                }
            }
        }
    };
}

operators! {
    Dollar = right(1),
    Assign = right(5),
    DoubleColon "::" = left(7),
    AddAssign "+=" = right(9),
    SubAssign "-=" = right(9),
    MulAssign "*=" = right(9),
    DivAssign "/=" = right(9),
    DoubleQuestionMark "??" = left(11),
    DoubleDot ".." = left(12),
    Or "or" = left(13),
    And "and" = left(15),
    Equals "==" = left(19),
    NotEquals "!=" = left(19),
    LeftShift "<<" = left(23),
    RightShift ">>" = left(23),
    LessThan "<" = left(25),
    LessThanEqual "<=" = left(25),
    GreaterThan ">" = left(25),
    GreaterThanEqual ">=" = left(25),
    BitAnd "&" = left(27),
    BitXor "^" = left(28),
    BitOr "|" = left(29),
    Plus "+" = left(31) => "__op_add",
    Minus "-" = left(31) => "__op_sub",
    Mul "*" = left(35) => "__op_mul",
    Div "/" = left(35) => "__op_div",
    IntDiv "./" = left(35) => "__op_idiv",
    Mod "%" = left(35) => "__op_mod",
    As "as" = left(37),
    Dot "." = left(51),
    Colon ":" = left(51),
    Bang "!",
    BitNot "~",
    OpenSquare "[",
}

impl TokenKind {
    pub fn prefix_bp(&self) -> Option<((), u8)> {
        use TokenKind::*;
        let val = match self {
            Minus | PlusPlus | MinusMinus | Bang | BitNot | BitXor => Some(41),
            _ => None,
        };

        Some(((), val?))
    }
}
