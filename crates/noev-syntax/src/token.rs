//! Tokens produced by the lexer.

use crate::span::Span;

/// One token: its kind and where it is. The text is recovered from the span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    /// What kind of token this is.
    pub kind: TokenKind,
    /// Where it is in the source.
    pub span: Span,
}

/// The kinds of token. Contextual keywords (`mv`, `rev`, `adj`, `ctrl`, `with`, `do`) are
/// lexed as [`TokenKind::Ident`]; the parser recognises them by position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
#[allow(missing_docs)] // Each variant is named for its spelling.
pub enum TokenKind {
    // Names and literals
    Ident,
    Int,
    Float,
    Ket,
    Bra,

    // Reserved keywords
    And,
    Assert,
    Break,
    Continue,
    Else,
    False,
    Fn,
    For,
    If,
    Impl,
    In,
    Let,
    Mut,
    Not,
    Or,
    Ref,
    Return,
    /// `self`
    SelfValue,
    /// `Self`
    SelfType,
    Struct,
    Trait,
    True,
    Type,
    Unsafe,
    While,

    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Amp,
    Pipe,
    Caret,
    Tilde,
    Shl,
    Shr,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    AmpEq,
    PipeEq,
    CaretEq,
    ShlEq,
    ShrEq,

    // Punctuation
    Dot,
    DotDot,
    Comma,
    Semi,
    Colon,
    ColonColon,
    Arrow,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,

    /// `!`, which is reserved. Lexed so the parser can suggest `not`.
    Bang,
    /// `&&`, not a Noev operator. Lexed so the parser can suggest `and`.
    AndAnd,
    /// `||`, not a Noev operator. Lexed so the parser can suggest `or`.
    OrOr,

    /// A line break that ends a statement (see `spec/grammar.md`, Line breaks).
    Newline,
    /// Text that is not a valid token. The lexer has already reported it.
    Error,
    /// The end of the file.
    Eof,
}

impl TokenKind {
    /// The reserved keyword spelled `text`, if any.
    pub fn keyword(text: &[u8]) -> Option<TokenKind> {
        use TokenKind::*;
        Some(match text {
            b"and" => And,
            b"assert" => Assert,
            b"break" => Break,
            b"continue" => Continue,
            b"else" => Else,
            b"false" => False,
            b"fn" => Fn,
            b"for" => For,
            b"if" => If,
            b"impl" => Impl,
            b"in" => In,
            b"let" => Let,
            b"mut" => Mut,
            b"not" => Not,
            b"or" => Or,
            b"ref" => Ref,
            b"return" => Return,
            b"self" => SelfValue,
            b"Self" => SelfType,
            b"struct" => Struct,
            b"trait" => Trait,
            b"true" => True,
            b"type" => Type,
            b"unsafe" => Unsafe,
            b"while" => While,
            _ => return None,
        })
    }

    /// Whether `|` after this token is bitwise or rather than the start of a ket.
    pub(crate) fn ends_operand(self) -> bool {
        use TokenKind::*;
        matches!(
            self,
            Ident | Int | Float | Ket | True | False | SelfValue | RParen | RBracket
        )
    }

    /// Whether a line break after this token can end a statement.
    pub(crate) fn ends_expression(self) -> bool {
        use TokenKind::*;
        self.ends_operand() || matches!(self, SelfType | RBrace | Break | Continue | Return)
    }

    /// Whether a line starting with this token continues the previous line.
    pub(crate) fn continues_line(self) -> bool {
        use TokenKind::*;
        matches!(
            self,
            Dot | DotDot
                | ColonColon
                | Colon
                | Comma
                | Arrow
                | Else
                | And
                | Or
                | AndAnd
                | OrOr
                | Plus
                | Star
                | Slash
                | Percent
                | Amp
                | Pipe
                | Caret
                | Shl
                | Shr
                | EqEq
                | NotEq
                | Lt
                | Le
                | Gt
                | Ge
                | Eq
                | PlusEq
                | MinusEq
                | StarEq
                | SlashEq
                | PercentEq
                | AmpEq
                | PipeEq
                | CaretEq
                | ShlEq
                | ShrEq
                | RParen
                | RBracket
                | RBrace
                | Eof
        )
    }
}
