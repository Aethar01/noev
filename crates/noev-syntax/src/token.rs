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
    Imag,
    Ket,
    Bra,
    /// A string literal, quotes included. Interpolations inside it are parsed separately.
    Str,

    // Reserved keywords
    And,
    As,
    Assert,
    Break,
    Continue,
    Else,
    Enum,
    Extern,
    False,
    Fn,
    For,
    If,
    Impl,
    In,
    Let,
    Match,
    Mut,
    Not,
    Or,
    Pub,
    Ref,
    Return,
    Sample,
    /// `self`
    SelfValue,
    /// `Self`
    SelfType,
    Struct,
    Trait,
    True,
    Type,
    Unsafe,
    Use,
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
    /// `=>`
    FatArrow,
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
            b"as" => As,
            b"assert" => Assert,
            b"break" => Break,
            b"continue" => Continue,
            b"else" => Else,
            b"enum" => Enum,
            b"extern" => Extern,
            b"false" => False,
            b"fn" => Fn,
            b"for" => For,
            b"if" => If,
            b"impl" => Impl,
            b"in" => In,
            b"let" => Let,
            b"match" => Match,
            b"mut" => Mut,
            b"not" => Not,
            b"or" => Or,
            b"pub" => Pub,
            b"ref" => Ref,
            b"return" => Return,
            b"sample" => Sample,
            b"self" => SelfValue,
            b"Self" => SelfType,
            b"struct" => Struct,
            b"trait" => Trait,
            b"true" => True,
            b"type" => Type,
            b"unsafe" => Unsafe,
            b"use" => Use,
            b"while" => While,
            _ => return None,
        })
    }

    /// Whether `|` after this token is bitwise or rather than the start of a ket.
    pub(crate) fn ends_operand(self) -> bool {
        use TokenKind::*;
        matches!(
            self,
            Ident | Int | Float | Imag | Ket | Str | True | False | SelfValue | RParen | RBracket
        )
    }

    /// Whether a line break after this token can end a statement.
    ///
    /// `>` closes an expectation value (`|ψ>`) as well as being a comparison, and `->` can
    /// end a closing ket such as `|+->`, so line breaks after them are kept. The parser skips
    /// them where an operand must follow, as in `a >` at the end of a line.
    pub(crate) fn ends_expression(self) -> bool {
        use TokenKind::*;
        self.ends_operand()
            || matches!(
                self,
                SelfType | RBrace | Break | Continue | Return | Gt | Arrow
            )
    }

    /// Whether a line starting with this token continues the previous line.
    ///
    /// `-` and `<` are not included: a line starting with them begins a new statement,
    /// since `-x` and `<ψ| H |ψ>` can start one.
    pub(crate) fn continues_line(self) -> bool {
        use TokenKind::*;
        matches!(
            self,
            Dot | DotDot
                | ColonColon
                | Colon
                | Comma
                | Arrow
                | FatArrow
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
