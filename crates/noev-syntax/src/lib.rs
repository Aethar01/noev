//! Lexer, parser and syntax tree for Noev source files.
//!
//! Implements `spec/grammar.md`.

pub mod ast;
pub mod diagnostic;
pub mod lexer;
pub mod parser;
pub mod span;
pub mod symbol;
pub mod token;

pub use ast::Ast;
pub use diagnostic::{Diagnostic, Severity};
pub use lexer::{Lexed, lex};
pub use parser::{Parsed, parse};
pub use span::Span;
pub use symbol::{FxHashMap, Interner, Symbol};
pub use token::{Token, TokenKind};
