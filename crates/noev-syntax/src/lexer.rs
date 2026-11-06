//! Turns source text into tokens.
//!
//! Lexing runs in two linear passes. The first splits the text into tokens, recording every
//! run of line breaks as one [`TokenKind::Newline`]. The second removes the line breaks that
//! do not end a statement, following `spec/grammar.md` (Line breaks), which needs the token
//! on each side and the enclosing bracket.

use crate::diagnostic::Diagnostic;
use crate::span::Span;
use crate::token::{Token, TokenKind};

/// The result of lexing a file.
#[derive(Debug)]
pub struct Lexed {
    /// The tokens, ending with [`TokenKind::Eof`].
    pub tokens: Vec<Token>,
    /// Lexical errors, such as unexpected characters.
    pub diagnostics: Vec<Diagnostic>,
}

/// Lexes `source`.
pub fn lex(source: &str) -> Lexed {
    let mut lexer = Lexer {
        bytes: source.as_bytes(),
        pos: 0,
        tokens: Vec::with_capacity(source.len() / 4 + 1),
        diagnostics: Vec::new(),
    };
    if u32::try_from(source.len()).is_err() {
        lexer.diagnostics.push(Diagnostic::error(
            Span::new(0, 0),
            "source file is larger than 4 GiB",
        ));
        lexer.push(TokenKind::Eof, 0);
        return lexer.finish();
    }
    lexer.run();
    let mut lexed = lexer.finish();
    filter_newlines(source.as_bytes(), &mut lexed.tokens);
    lexed
}

struct Lexer<'a> {
    bytes: &'a [u8],
    pos: usize,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
}

impl Lexer<'_> {
    fn finish(self) -> Lexed {
        Lexed {
            tokens: self.tokens,
            diagnostics: self.diagnostics,
        }
    }

    fn peek(&self, offset: usize) -> u8 {
        self.bytes.get(self.pos + offset).copied().unwrap_or(0)
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.tokens.push(Token {
            kind,
            span: Span::new(start as u32, self.pos as u32),
        });
    }

    /// Records a line break, merging it with an immediately preceding one.
    fn push_newline(&mut self, start: usize) {
        if self
            .tokens
            .last()
            .is_some_and(|t| t.kind == TokenKind::Newline)
        {
            return;
        }
        self.push(TokenKind::Newline, start);
    }

    fn last_kind(&self) -> Option<TokenKind> {
        self.tokens.last().map(|t| t.kind)
    }

    fn error(&mut self, start: usize, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic::error(
            Span::new(start as u32, self.pos as u32),
            message,
        ));
    }

    fn run(&mut self) {
        while self.pos < self.bytes.len() {
            let start = self.pos;
            let byte = self.bytes[start];
            match byte {
                b' ' | b'\t' | b'\r' => self.pos += 1,
                b'\n' => {
                    self.pos += 1;
                    self.push_newline(start);
                }
                b'/' if self.peek(1) == b'/' => self.line_comment(),
                b'/' if self.peek(1) == b'*' => self.block_comment(),
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.ident(),
                b'0'..=b'9' => self.number(),
                b'|' if self.ket_allowed() && self.state_literal(b'|', b'>', TokenKind::Ket) => {}
                b'<' if self.ket_allowed() && self.state_literal(b'<', b'|', TokenKind::Bra) => {}
                _ if byte.is_ascii() => self.punct(),
                _ => {
                    let len = std::str::from_utf8(&self.bytes[start..])
                        .ok()
                        .and_then(|rest| rest.chars().next())
                        .map_or(1, char::len_utf8);
                    self.pos += len;
                    self.error(start, "unexpected character");
                    self.push(TokenKind::Error, start);
                }
            }
        }
        self.push(TokenKind::Eof, self.pos);
    }

    fn line_comment(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
            self.pos += 1;
        }
    }

    /// Skips a nested block comment. A comment spanning lines counts as a line break.
    fn block_comment(&mut self) {
        let start = self.pos;
        self.pos += 2;
        let mut depth = 1;
        let mut spans_lines = false;
        while depth > 0 {
            match (self.peek(0), self.peek(1)) {
                (b'*', b'/') => {
                    depth -= 1;
                    self.pos += 2;
                }
                (b'/', b'*') => {
                    depth += 1;
                    self.pos += 2;
                }
                _ if self.pos >= self.bytes.len() => {
                    self.error(start, "unterminated block comment");
                    return;
                }
                (b'\n', _) => {
                    spans_lines = true;
                    self.pos += 1;
                }
                _ => self.pos += 1,
            }
        }
        if spans_lines {
            self.push_newline(start);
        }
    }

    fn ident(&mut self) {
        let start = self.pos;
        while self.peek(0).is_ascii_alphanumeric() || self.peek(0) == b'_' {
            self.pos += 1;
        }
        let kind = TokenKind::keyword(&self.bytes[start..self.pos]).unwrap_or(TokenKind::Ident);
        self.push(kind, start);
    }

    fn number(&mut self) {
        let start = self.pos;
        // After `.`, digits are a tuple index: `p.0.1` is `p . 0 . 1`, not `p . 0.1`.
        if self.last_kind() == Some(TokenKind::Dot) {
            self.digits(u8::is_ascii_digit);
            return self.push(TokenKind::Int, start);
        }
        let radix_digit: Option<fn(&u8) -> bool> = match (self.peek(0), self.peek(1)) {
            (b'0', b'x') => Some(u8::is_ascii_hexdigit),
            (b'0', b'b') => Some(|b| matches!(b, b'0' | b'1')),
            _ => None,
        };
        if let Some(is_digit) = radix_digit {
            self.pos += 2;
            if self.digits(is_digit) == 0 {
                self.error(start, "expected digits after the number prefix");
            }
            return self.push(TokenKind::Int, start);
        }

        self.digits(u8::is_ascii_digit);
        let mut kind = TokenKind::Int;
        if self.peek(0) == b'.' && self.peek(1).is_ascii_digit() {
            self.pos += 1;
            self.digits(u8::is_ascii_digit);
            kind = TokenKind::Float;
        }
        if matches!(self.peek(0), b'e' | b'E') {
            let sign = usize::from(matches!(self.peek(1), b'+' | b'-'));
            if self.peek(1 + sign).is_ascii_digit() {
                self.pos += 1 + sign;
                self.digits(u8::is_ascii_digit);
                kind = TokenKind::Float;
            }
        }
        self.push(kind, start);
    }

    /// Consumes digits and `_` separators, returning how many digits there were.
    fn digits(&mut self, is_digit: fn(&u8) -> bool) -> usize {
        let mut count = 0;
        loop {
            let byte = self.peek(0);
            if is_digit(&byte) {
                count += 1;
            } else if byte != b'_' {
                return count;
            }
            self.pos += 1;
        }
    }

    /// Whether a ket or bra may start here: anywhere an operand can, which is anywhere
    /// except directly after a token that ends one on the same line.
    fn ket_allowed(&self) -> bool {
        !self.last_kind().is_some_and(TokenKind::ends_operand)
    }

    /// Lexes a ket `|…>` or bra `<…|` (between `open` and `close`, one or more of `0 1 + -`)
    /// if it is there, returning whether it was.
    fn state_literal(&mut self, open: u8, close: u8, kind: TokenKind) -> bool {
        debug_assert_eq!(self.peek(0), open);
        let start = self.pos;
        let mut end = start + 1;
        while matches!(self.bytes.get(end), Some(b'0' | b'1' | b'+' | b'-')) {
            end += 1;
        }
        if end == start + 1 || self.bytes.get(end) != Some(&close) {
            return false;
        }
        self.pos = end + 1;
        self.push(kind, start);
        true
    }

    fn punct(&mut self) {
        use TokenKind::*;
        let start = self.pos;
        let (kind, len) = match (self.peek(0), self.peek(1), self.peek(2)) {
            (b'<', b'<', b'=') => (ShlEq, 3),
            (b'>', b'>', b'=') => (ShrEq, 3),
            (b'<', b'<', _) => (Shl, 2),
            (b'>', b'>', _) => (Shr, 2),
            (b'=', b'=', _) => (EqEq, 2),
            (b'!', b'=', _) => (NotEq, 2),
            (b'<', b'=', _) => (Le, 2),
            (b'>', b'=', _) => (Ge, 2),
            (b'+', b'=', _) => (PlusEq, 2),
            (b'-', b'=', _) => (MinusEq, 2),
            (b'*', b'=', _) => (StarEq, 2),
            (b'/', b'=', _) => (SlashEq, 2),
            (b'%', b'=', _) => (PercentEq, 2),
            (b'&', b'=', _) => (AmpEq, 2),
            (b'|', b'=', _) => (PipeEq, 2),
            (b'^', b'=', _) => (CaretEq, 2),
            (b'&', b'&', _) => (AndAnd, 2),
            (b'|', b'|', _) => (OrOr, 2),
            (b'-', b'>', _) => (Arrow, 2),
            (b':', b':', _) => (ColonColon, 2),
            (b'.', b'.', _) => (DotDot, 2),
            (b'+', ..) => (Plus, 1),
            (b'-', ..) => (Minus, 1),
            (b'*', ..) => (Star, 1),
            (b'/', ..) => (Slash, 1),
            (b'%', ..) => (Percent, 1),
            (b'&', ..) => (Amp, 1),
            (b'|', ..) => (Pipe, 1),
            (b'^', ..) => (Caret, 1),
            (b'~', ..) => (Tilde, 1),
            (b'!', ..) => (Bang, 1),
            (b'<', ..) => (Lt, 1),
            (b'>', ..) => (Gt, 1),
            (b'=', ..) => (Eq, 1),
            (b'.', ..) => (Dot, 1),
            (b',', ..) => (Comma, 1),
            (b';', ..) => (Semi, 1),
            (b':', ..) => (Colon, 1),
            (b'(', ..) => (LParen, 1),
            (b')', ..) => (RParen, 1),
            (b'[', ..) => (LBracket, 1),
            (b']', ..) => (RBracket, 1),
            (b'{', ..) => (LBrace, 1),
            (b'}', ..) => (RBrace, 1),
            _ => {
                self.pos += 1;
                self.error(start, "unexpected character");
                return self.push(Error, start);
            }
        };
        self.pos += len;
        self.push(kind, start);
    }
}

/// Removes the line breaks that do not end a statement.
fn filter_newlines(source: &[u8], tokens: &mut Vec<Token>) {
    use TokenKind::*;
    let mut brackets: Vec<TokenKind> = Vec::new();
    let mut kept: usize = 0;
    for i in 0..tokens.len() {
        let token = tokens[i];
        match token.kind {
            LParen | LBracket | LBrace => brackets.push(token.kind),
            RParen | RBracket | RBrace => {
                brackets.pop();
            }
            Newline => {
                let in_parens = matches!(brackets.last(), Some(LParen | LBracket));
                let after = kept.checked_sub(1).map(|j| tokens[j].kind);
                let next = tokens[i + 1];
                // `do` is contextual: it continues the line only after a `with` block's `}`.
                let next_is_do = after == Some(RBrace)
                    && next.kind == Ident
                    && &source[next.span.start as usize..next.span.end as usize] == b"do";
                let ends = !in_parens
                    && after.is_some_and(TokenKind::ends_expression)
                    && !next.kind.continues_line()
                    && !next_is_do;
                if !ends {
                    continue;
                }
            }
            _ => {}
        }
        tokens[kept] = token;
        kept += 1;
    }
    tokens.truncate(kept);
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenKind::*;

    fn kinds(source: &str) -> Vec<TokenKind> {
        let lexed = lex(source);
        assert!(lexed.diagnostics.is_empty(), "{:?}", lexed.diagnostics);
        lexed
            .tokens
            .iter()
            .map(|t| t.kind)
            .filter(|&k| k != Eof)
            .collect()
    }

    fn texts(source: &str) -> Vec<&str> {
        lex(source)
            .tokens
            .iter()
            .filter(|t| t.kind != Eof)
            .map(|t| t.span.text(source))
            .collect()
    }

    #[test]
    fn keywords_and_contextual_keywords() {
        assert_eq!(kinds("let mut x"), [Let, Mut, Ident]);
        assert_eq!(kinds("mv rev adj ctrl with do"), [Ident; 6]);
        assert_eq!(kinds("self Self"), [SelfValue, SelfType]);
    }

    #[test]
    fn kets_in_operand_position() {
        assert_eq!(kinds("let q = |0>"), [Let, Ident, Eq, Ket]);
        assert_eq!(texts("|+0->"), ["|+0->"]);
        assert_eq!(kinds("(|0>, |1>)"), [LParen, Ket, Comma, Ket, RParen]);
    }

    #[test]
    fn pipe_after_an_operand_is_bitwise_or() {
        assert_eq!(kinds("a | 0"), [Ident, Pipe, Int]);
        assert_eq!(
            kinds("(a) |0> b"),
            [LParen, Ident, RParen, Pipe, Int, Gt, Ident]
        );
        assert_eq!(kinds("a |= b"), [Ident, PipeEq, Ident]);
    }

    #[test]
    fn pipe_that_is_not_a_ket() {
        assert_eq!(kinds("x = |2>"), [Ident, Eq, Pipe, Int, Gt]);
        assert_eq!(kinds("|| x"), [OrOr, Ident]);
    }

    #[test]
    fn numbers() {
        assert_eq!(kinds("1_000 0xff 0b1010"), [Int, Int, Int]);
        assert_eq!(kinds("1.0 3.14 1e-3 2.5e6 1E+9"), [Float; 5]);
        assert_eq!(kinds("0..3"), [Int, DotDot, Int]);
        assert_eq!(kinds("1.max"), [Int, Dot, Ident]);
    }

    #[test]
    fn tuple_indices() {
        assert_eq!(texts("p.0.1"), ["p", ".", "0", ".", "1"]);
    }

    #[test]
    fn operators() {
        assert_eq!(
            kinds("<<= >>= << >> -> :: .."),
            [ShlEq, ShrEq, Shl, Shr, Arrow, ColonColon, DotDot]
        );
        assert_eq!(kinds("! != && ||"), [Bang, NotEq, AndAnd, OrOr]);
    }

    #[test]
    fn comments() {
        assert_eq!(kinds("a // note\nb"), [Ident, Newline, Ident]);
        assert_eq!(kinds("a /* x /* nested */ y */ b"), [Ident, Ident]);
        assert_eq!(kinds("a /* spans\nlines */ b"), [Ident, Newline, Ident]);
    }

    #[test]
    fn newline_ends_a_statement() {
        assert_eq!(kinds("a\nb"), [Ident, Newline, Ident]);
        assert_eq!(kinds("a\n\n\nb"), [Ident, Newline, Ident]);
        assert_eq!(
            kinds("q.h()\nq"),
            [Ident, Dot, Ident, LParen, RParen, Newline, Ident]
        );
    }

    #[test]
    fn newline_after_an_unfinished_expression() {
        assert_eq!(kinds("let x =\n5"), [Let, Ident, Eq, Int]);
        assert_eq!(kinds("a +\nb"), [Ident, Plus, Ident]);
    }

    #[test]
    fn newline_before_a_continuation() {
        assert_eq!(kinds("a\n+ b"), [Ident, Plus, Ident]);
        assert_eq!(kinds("q\n.h()"), [Ident, Dot, Ident, LParen, RParen]);
        assert_eq!(kinds("}\nelse"), [RBrace, Else]);
        assert_eq!(kinds("}\ndo"), [RBrace, Ident]);
        assert_eq!(kinds("x\ndo"), [Ident, Newline, Ident]);
    }

    #[test]
    fn newline_after_closing_angle_is_kept() {
        assert_eq!(
            kinds("f(x)>\ny"),
            [Ident, LParen, Ident, RParen, Gt, Newline, Ident]
        );
        assert_eq!(kinds("|+->\ny"), [Ket, Newline, Ident]);
    }

    #[test]
    fn leading_minus_starts_a_statement() {
        assert_eq!(kinds("a\n-b"), [Ident, Newline, Minus, Ident]);
    }

    #[test]
    fn leading_less_than_starts_a_statement() {
        assert_eq!(kinds("a\n<p()| h |p()>")[..3], [Ident, Newline, Lt]);
    }

    #[test]
    fn newlines_inside_brackets() {
        assert_eq!(
            kinds("f(a\n, b\n)"),
            [Ident, LParen, Ident, Comma, Ident, RParen]
        );
        assert_eq!(kinds("[1\n2]"), [LBracket, Int, Int, RBracket]);
        assert_eq!(
            kinds("f({\na\nb\n})"),
            [Ident, LParen, LBrace, Ident, Newline, Ident, RBrace, RParen]
        );
    }

    #[test]
    fn bras_in_operand_position() {
        assert_eq!(
            kinds("let hit = <1| mv q"),
            [Let, Ident, Eq, Bra, Ident, Ident]
        );
        assert_eq!(texts("<+0-|"), ["<+0-|"]);
        assert_eq!(
            kinds("<+| Z(0) |+>"),
            [Bra, Ident, LParen, Int, RParen, Pipe, Plus, Gt]
        );
    }

    #[test]
    fn less_than_after_an_operand() {
        assert_eq!(kinds("a < 0|b"), [Ident, Lt, Int, Pipe, Ident]);
        assert_eq!(kinds("x = <2|"), [Ident, Eq, Lt, Int, Pipe]);
    }

    #[test]
    fn ket_at_line_start() {
        assert_eq!(kinds("a\n|0>"), [Ident, Newline, Ket]);
    }

    #[test]
    fn errors() {
        let lexed = lex("a € b");
        assert_eq!(lexed.diagnostics.len(), 1);
        assert_eq!(lexed.diagnostics[0].span, Span::new(2, 5));
        assert_eq!(lex("/* open").diagnostics.len(), 1);
        assert_eq!(lex("0x").diagnostics.len(), 1);
    }

    #[test]
    fn ui_tests_lex_cleanly() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/ui");
        let mut stack = vec![std::path::PathBuf::from(dir)];
        let mut count = 0;
        while let Some(path) = stack.pop() {
            if path.is_dir() {
                stack.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            } else if path.extension().is_some_and(|e| e == "nv") {
                let source = std::fs::read_to_string(&path).unwrap();
                let lexed = lex(&source);
                assert!(
                    lexed.diagnostics.is_empty(),
                    "{}: {:?}",
                    path.display(),
                    lexed.diagnostics
                );
                count += 1;
            }
        }
        assert!(count > 0);
    }
}
