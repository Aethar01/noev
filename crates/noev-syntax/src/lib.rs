//! Lexer, parser and syntax tree for Noev source files.
//!
//! Implements `spec/grammar.md`.

pub mod diagnostic;
pub mod lexer;
pub mod span;
pub mod token;

pub use diagnostic::{Diagnostic, Severity};
pub use lexer::{Lexed, lex};
pub use span::Span;
pub use token::{Token, TokenKind};
