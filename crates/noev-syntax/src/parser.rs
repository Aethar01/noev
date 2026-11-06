//! Turns tokens into a syntax tree.
//!
//! A hand-written recursive-descent parser, with precedence climbing for binary operators.
//! It never backtracks: every decision is made from the current token and at most two more.
//! After an error it records a diagnostic, produces an error node, and skips to the end of
//! the statement or item, so one mistake does not hide the rest of the file.

use crate::ast::*;
use crate::diagnostic::Diagnostic;
use crate::lexer::lex;
use crate::span::Span;
use crate::symbol::Symbol;
use crate::token::{Token, TokenKind};

/// The result of parsing a file.
#[derive(Debug)]
pub struct Parsed {
    /// The syntax tree. Parts that could not be parsed are error nodes.
    pub ast: Ast,
    /// Lexical and syntax errors.
    pub diagnostics: Vec<Diagnostic>,
}

/// Lexes and parses `source`.
pub fn parse(source: &str) -> Parsed {
    let lexed = lex(source);
    let mut parser = Parser {
        source,
        tokens: &lexed.tokens,
        pos: 0,
        ast: Ast::default(),
        diagnostics: lexed.diagnostics,
        no_struct: false,
    };
    parser.program();
    Parsed {
        ast: parser.ast,
        diagnostics: parser.diagnostics,
    }
}

/// Binding strength of the binary operators between `|` and `*`, from loosest to tightest.
/// `or`, `and`, `not` and comparisons are looser and handled by their own functions.
fn binary_op(kind: TokenKind) -> Option<(BinaryOp, u8)> {
    use TokenKind::*;
    Some(match kind {
        Pipe => (BinaryOp::BitOr, PREC_BIT_OR),
        Caret => (BinaryOp::BitXor, PREC_BIT_XOR),
        Amp => (BinaryOp::BitAnd, 3),
        Shl => (BinaryOp::Shl, 4),
        Shr => (BinaryOp::Shr, 4),
        Plus => (BinaryOp::Add, 5),
        Minus => (BinaryOp::Sub, 5),
        Star => (BinaryOp::Mul, 6),
        Slash => (BinaryOp::Div, 6),
        Percent => (BinaryOp::Rem, 6),
        _ => return None,
    })
}

const PREC_BIT_OR: u8 = 1;
/// Observables in expectation values bind tighter than `|`, so the `|` that closes them is
/// never read as bitwise or.
const PREC_BIT_XOR: u8 = 2;

fn comparison_op(kind: TokenKind) -> Option<BinaryOp> {
    use TokenKind::*;
    Some(match kind {
        EqEq => BinaryOp::Eq,
        NotEq => BinaryOp::NotEq,
        Lt => BinaryOp::Lt,
        Le => BinaryOp::Le,
        Gt => BinaryOp::Gt,
        Ge => BinaryOp::Ge,
        _ => return None,
    })
}

fn assign_op(kind: TokenKind) -> Option<AssignOp> {
    use TokenKind::*;
    let op = match kind {
        Eq => return Some(AssignOp::Assign),
        PlusEq => BinaryOp::Add,
        MinusEq => BinaryOp::Sub,
        StarEq => BinaryOp::Mul,
        SlashEq => BinaryOp::Div,
        PercentEq => BinaryOp::Rem,
        AmpEq => BinaryOp::BitAnd,
        PipeEq => BinaryOp::BitOr,
        CaretEq => BinaryOp::BitXor,
        ShlEq => BinaryOp::Shl,
        ShrEq => BinaryOp::Shr,
        _ => return None,
    };
    Some(AssignOp::Compound(op))
}

struct Parser<'a> {
    source: &'a str,
    tokens: &'a [Token],
    pos: usize,
    ast: Ast,
    diagnostics: Vec<Diagnostic>,
    /// Whether a struct literal is disallowed here, as in `if x { … }`.
    no_struct: bool,
}

// Token access.
impl Parser<'_> {
    fn nth(&self, n: usize) -> Token {
        self.tokens[(self.pos + n).min(self.tokens.len() - 1)]
    }

    fn kind(&self) -> TokenKind {
        self.nth(0).kind
    }

    fn span(&self) -> Span {
        self.nth(0).span
    }

    fn prev_span(&self) -> Span {
        self.tokens[self.pos.saturating_sub(1)].span
    }

    fn at(&self, kind: TokenKind) -> bool {
        self.kind() == kind
    }

    fn bump(&mut self) -> Token {
        let token = self.nth(0);
        if token.kind != TokenKind::Eof {
            self.pos += 1;
        }
        token
    }

    fn eat(&mut self, kind: TokenKind) -> bool {
        let found = self.at(kind);
        if found {
            self.bump();
        }
        found
    }

    fn text(&self, span: Span) -> &str {
        span.text(self.source)
    }

    /// Whether the token `n` ahead is the identifier `word` (for contextual keywords).
    fn nth_is_word(&self, n: usize, word: &str) -> bool {
        let token = self.nth(n);
        token.kind == TokenKind::Ident && self.text(token.span) == word
    }

    /// Whether the token `n` ahead can start a place.
    fn nth_starts_place(&self, n: usize) -> bool {
        matches!(self.nth(n).kind, TokenKind::Ident | TokenKind::SelfValue)
    }

    fn at_statement_end(&self) -> bool {
        use TokenKind::*;
        matches!(self.kind(), Newline | Semi | RBrace | Eof)
    }

    fn skip_newlines(&mut self) {
        while self.eat(TokenKind::Newline) {}
    }

    fn skip_separators(&mut self) {
        while matches!(self.kind(), TokenKind::Newline | TokenKind::Semi) {
            self.bump();
        }
    }

    fn intern(&mut self, span: Span) -> Symbol {
        self.ast.symbols.intern(span.text(self.source))
    }
}

// Errors and recovery.
impl Parser<'_> {
    fn found(&self) -> String {
        match self.kind() {
            TokenKind::Newline => "a line break".into(),
            TokenKind::Eof => "the end of the file".into(),
            _ => format!("`{}`", self.text(self.span())),
        }
    }

    fn error(&mut self, span: Span, message: impl Into<String>) -> &mut Diagnostic {
        self.diagnostics.push(Diagnostic::error(span, message));
        self.diagnostics.last_mut().expect("just pushed")
    }

    fn expected(&mut self, what: &str) -> &mut Diagnostic {
        let message = format!("expected {what}, found {}", self.found());
        let span = self.span();
        self.error(span, message)
    }

    fn expect(&mut self, kind: TokenKind, what: &str) -> bool {
        if self.eat(kind) {
            return true;
        }
        self.expected(what);
        false
    }

    /// Skips to the end of the current statement: a line break, `;` or `}` outside any
    /// brackets opened while skipping. The terminator is not consumed.
    fn recover_statement(&mut self) {
        use TokenKind::*;
        let mut depth = 0usize;
        loop {
            match self.kind() {
                Eof => return,
                Newline | Semi if depth == 0 => return,
                RBrace | RParen | RBracket if depth == 0 => return,
                LParen | LBracket | LBrace => depth += 1,
                RParen | RBracket | RBrace => depth -= 1,
                _ => {}
            }
            self.bump();
        }
    }

    /// Skips a bad member of a struct, trait or `impl` body, always making progress.
    fn recover_member(&mut self) {
        let before = self.pos;
        self.recover_statement();
        if self.pos == before && !self.at(TokenKind::RBrace) {
            self.bump();
        }
    }

    /// Skips to the start of the next item.
    fn recover_item(&mut self) {
        use TokenKind::*;
        let mut depth = 0usize;
        self.bump();
        loop {
            match self.kind() {
                Eof => return,
                Fn | Struct | Trait | Impl | Unsafe | Extern if depth == 0 => return,
                Ident if depth == 0 && self.nth_is_word(0, "rev") && self.nth(1).kind == Fn => {
                    return;
                }
                LParen | LBracket | LBrace => depth += 1,
                RParen | RBracket | RBrace => depth = depth.saturating_sub(1),
                _ => {}
            }
            self.bump();
        }
    }

    fn error_expr(&mut self, span: Span) -> ExprId {
        self.ast.push_expr(Expr {
            kind: ExprKind::Error,
            span,
        })
    }
}

// Items.
impl Parser<'_> {
    fn program(&mut self) {
        let mark = self.ast.mark::<ItemId>();
        loop {
            self.skip_separators();
            if self.at(TokenKind::Eof) {
                break;
            }
            match self.item() {
                Some(item) => self.ast.push_to(item),
                None => {
                    self.expected("an item (`fn`, `struct`, `trait` or `impl`)");
                    self.recover_item();
                }
            }
        }
        self.ast.root = self.ast.finish(mark);
    }

    fn at_fn_start(&self) -> bool {
        use TokenKind::*;
        match self.kind() {
            Fn | Unsafe => true,
            Ident => self.nth_is_word(0, "rev") && self.nth(1).kind == Fn,
            _ => false,
        }
    }

    fn item(&mut self) -> Option<ItemId> {
        let start = self.span();
        let kind = match self.kind() {
            _ if self.at_fn_start() => ItemKind::Fn(self.fn_decl(true)),
            TokenKind::Struct => self.struct_def(),
            TokenKind::Trait => self.trait_def(),
            TokenKind::Impl => self.impl_def(),
            TokenKind::Extern => {
                let span = self.span();
                self.error(span, "`extern` functions are not supported yet")
                    .with_help_mut("see spec/interop.md for what is planned");
                self.bump();
                // Read the declaration so it is not reported again as a broken item.
                if self.at_fn_start() {
                    self.fn_decl(false);
                } else {
                    self.recover_item();
                }
                return Some(self.ast.push_item(Item {
                    kind: ItemKind::Error,
                    span: start.to(self.prev_span()),
                }));
            }
            _ => return None,
        };
        let span = start.to(self.prev_span());
        Some(self.ast.push_item(Item { kind, span }))
    }

    fn name(&mut self, what: &str) -> (Symbol, Span) {
        let span = self.span();
        if self.eat(TokenKind::Ident) {
            (self.intern(span), span)
        } else {
            if !self.reserved_word() {
                self.expected(what);
            }
            (self.ast.symbols.intern("<error>"), span)
        }
    }

    /// Reports and skips a reserved keyword used where a name is expected, returning whether
    /// there was one.
    fn reserved_word(&mut self) -> bool {
        let span = self.span();
        let text = self.text(span);
        if TokenKind::keyword(text.as_bytes()).is_none() {
            return false;
        }
        let message = format!("`{text}` is a reserved keyword and cannot be used as a name");
        self.error(span, message);
        self.bump();
        true
    }

    /// `[unsafe] [rev] fn name(params) [-> type] block`, or without the block when
    /// `needs_body` is false (trait declarations).
    fn fn_decl(&mut self, needs_body: bool) -> FnId {
        let start = self.span();
        let is_unsafe = self.eat(TokenKind::Unsafe);
        let is_rev = self.nth_is_word(0, "rev");
        if is_rev {
            self.bump();
        }
        self.expect(TokenKind::Fn, "`fn`");
        let (name, name_span) = self.name("a function name");

        let mut receiver = None;
        let mark = self.ast.mark::<Param>();
        if self.expect(TokenKind::LParen, "`(`") {
            let mut first = true;
            while !matches!(self.kind(), TokenKind::RParen | TokenKind::Eof) {
                if first && let Some(r) = self.receiver() {
                    receiver = Some(r);
                } else {
                    let param = self.param();
                    self.ast.push_to(param);
                }
                first = false;
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
            self.expect(TokenKind::RParen, "`)` or `,`");
        }
        let params = self.ast.finish(mark);

        let ret = self.eat(TokenKind::Arrow).then(|| {
            self.skip_newlines();
            self.ty()
        });
        let body = if needs_body || self.at(TokenKind::LBrace) {
            self.skip_newlines();
            Some(self.block())
        } else {
            None
        };
        let span = start.to(self.prev_span());
        self.ast.push_fn(FnDecl {
            name,
            name_span,
            is_unsafe,
            is_rev,
            receiver,
            params,
            ret,
            body,
            span,
        })
    }

    /// `mv self`, `ref self` or `mut self`, if one is here.
    fn receiver(&mut self) -> Option<Receiver> {
        use TokenKind::*;
        let start = self.span();
        let kind = match self.kind() {
            Ref if self.nth(1).kind == SelfValue => OwnKind::Ref,
            Mut if self.nth(1).kind == SelfValue => OwnKind::Mut,
            Ident if self.nth_is_word(0, "mv") && self.nth(1).kind == SelfValue => OwnKind::Mv,
            SelfValue => {
                self.bump();
                self.error(start, "say how the method takes `self`")
                    .with_help_mut("write `ref self` to read it, `mut self` to change it, or `mv self` to take ownership");
                return Some(Receiver {
                    kind: OwnKind::Ref,
                    span: start,
                });
            }
            _ => return None,
        };
        self.bump();
        self.bump();
        Some(Receiver {
            kind,
            span: start.to(self.prev_span()),
        })
    }

    fn param(&mut self) -> Param {
        let start = self.span();
        let (name, _) = self.name("a parameter name");
        self.expect(TokenKind::Colon, "`:` and the parameter's type");
        let ty = self.ty();
        Param {
            name,
            ty,
            span: start.to(self.prev_span()),
        }
    }

    fn struct_def(&mut self) -> ItemKind {
        self.bump();
        let (name, name_span) = self.name("a struct name");
        let field_mark = self.ast.mark::<FieldDecl>();
        let method_mark = self.ast.mark::<FnId>();
        self.skip_newlines();
        if self.expect(TokenKind::LBrace, "`{`") {
            loop {
                while matches!(
                    self.kind(),
                    TokenKind::Newline | TokenKind::Semi | TokenKind::Comma
                ) {
                    self.bump();
                }
                if matches!(self.kind(), TokenKind::RBrace | TokenKind::Eof) {
                    break;
                }
                if self.at_fn_start() {
                    let method = self.fn_decl(true);
                    self.ast.push_to(method);
                } else if self.at(TokenKind::Ident) {
                    let start = self.span();
                    let (name, _) = self.name("a field name");
                    self.expect(TokenKind::Colon, "`:` and the field's type");
                    let ty = self.ty();
                    let span = start.to(self.prev_span());
                    self.ast.push_to(FieldDecl { name, ty, span });
                } else {
                    self.expected("a field or `fn`");
                    self.recover_member();
                }
            }
            self.expect(TokenKind::RBrace, "`}`");
        }
        ItemKind::Struct {
            name,
            name_span,
            fields: self.ast.finish(field_mark),
            methods: self.ast.finish(method_mark),
        }
    }

    fn trait_def(&mut self) -> ItemKind {
        self.bump();
        let (name, _) = self.name("a trait name");
        let fn_mark = self.ast.mark::<FnId>();
        let type_mark = self.ast.mark::<Symbol>();
        self.skip_newlines();
        if self.expect(TokenKind::LBrace, "`{`") {
            loop {
                self.skip_separators();
                if matches!(self.kind(), TokenKind::RBrace | TokenKind::Eof) {
                    break;
                }
                if self.at_fn_start() {
                    let f = self.fn_decl(false);
                    self.ast.push_to(f);
                } else if self.eat(TokenKind::Type) {
                    let (name, _) = self.name("an associated type name");
                    self.ast.push_to(name);
                } else {
                    self.expected("`fn` or `type`");
                    self.recover_member();
                }
            }
            self.expect(TokenKind::RBrace, "`}`");
        }
        ItemKind::Trait {
            name,
            fns: self.ast.finish(fn_mark),
            types: self.ast.finish(type_mark),
        }
    }

    fn impl_def(&mut self) -> ItemKind {
        self.bump();
        let trait_path = self.path_segments();
        self.expect(TokenKind::For, "`for`");
        let ty = self.ty();
        let fn_mark = self.ast.mark::<FnId>();
        let alias_mark = self.ast.mark::<TypeAlias>();
        self.skip_newlines();
        if self.expect(TokenKind::LBrace, "`{`") {
            loop {
                self.skip_separators();
                if matches!(self.kind(), TokenKind::RBrace | TokenKind::Eof) {
                    break;
                }
                if self.at_fn_start() {
                    let f = self.fn_decl(true);
                    self.ast.push_to(f);
                } else if self.eat(TokenKind::Type) {
                    let (name, _) = self.name("an associated type name");
                    self.expect(TokenKind::Eq, "`=`");
                    let ty = self.ty();
                    self.ast.push_to(TypeAlias { name, ty });
                } else {
                    self.expected("`fn` or `type`");
                    self.recover_member();
                }
            }
            self.expect(TokenKind::RBrace, "`}`");
        }
        ItemKind::Impl {
            trait_path,
            ty,
            fns: self.ast.finish(fn_mark),
            types: self.ast.finish(alias_mark),
        }
    }

    /// `Name`, `Self` or `a::b::c`.
    fn path_segments(&mut self) -> List<Symbol> {
        let mark = self.ast.mark::<Symbol>();
        loop {
            let span = self.span();
            if matches!(
                self.kind(),
                TokenKind::Ident | TokenKind::SelfType | TokenKind::SelfValue
            ) {
                self.bump();
                let symbol = self.intern(span);
                self.ast.push_to(symbol);
            } else {
                self.expected("a name");
                break;
            }
            if !self.eat(TokenKind::ColonColon) {
                break;
            }
        }
        self.ast.finish(mark)
    }
}

// Types and patterns.
impl Parser<'_> {
    fn ty(&mut self) -> TypeId {
        use TokenKind::*;
        let start = self.span();
        let kind = match self.kind() {
            Ref | Mut => {
                let mutable = self.bump().kind == Mut;
                let inner = self.ty();
                TypeKind::Ref { mutable, inner }
            }
            LParen => {
                self.bump();
                if self.eat(RParen) {
                    TypeKind::Unit
                } else {
                    let mark = self.ast.mark::<TypeId>();
                    let first = self.ty();
                    if !self.at(Comma) {
                        self.expect(RParen, "`)`");
                        return first;
                    }
                    self.ast.push_to(first);
                    while self.eat(Comma) && !self.at(RParen) {
                        let ty = self.ty();
                        self.ast.push_to(ty);
                    }
                    self.expect(RParen, "`)` or `,`");
                    TypeKind::Tuple(self.ast.finish(mark))
                }
            }
            LBracket => {
                self.bump();
                let elem = self.ty();
                self.expect(Semi, "`;` and the array's length");
                let len_span = self.span();
                let len = if self.eat(Int) {
                    self.int_value(len_span).unwrap_or(0)
                } else {
                    self.expected("the array's length");
                    0
                };
                self.expect(RBracket, "`]`");
                TypeKind::Array { elem, len }
            }
            Ident | SelfType => TypeKind::Path(self.path_segments()),
            _ => {
                self.expected("a type");
                TypeKind::Error
            }
        };
        let span = start.to(self.prev_span());
        self.ast.push_type(TypeExpr { kind, span })
    }

    fn pat(&mut self) -> PatId {
        use TokenKind::*;
        let start = self.span();
        let kind = match self.kind() {
            Ident if self.text(start) == "_" => {
                self.bump();
                PatKind::Wildcard
            }
            Ident => {
                self.bump();
                PatKind::Name(self.intern(start))
            }
            LParen => {
                self.bump();
                let mark = self.ast.mark::<PatId>();
                while !matches!(self.kind(), RParen | Eof) {
                    let pat = self.pat();
                    self.ast.push_to(pat);
                    if !self.eat(Comma) {
                        break;
                    }
                }
                self.expect(RParen, "`)` or `,`");
                PatKind::Tuple(self.ast.finish(mark))
            }
            _ => {
                if !self.reserved_word() {
                    self.expected("a variable name or `(`");
                }
                PatKind::Wildcard
            }
        };
        let span = start.to(self.prev_span());
        self.ast.push_pat(Pat { kind, span })
    }
}

// Blocks and statements.
impl Parser<'_> {
    fn block(&mut self) -> BlockId {
        let start = self.span();
        let mark = self.ast.mark::<StmtId>();
        if self.expect(TokenKind::LBrace, "`{`") {
            let saved = std::mem::replace(&mut self.no_struct, false);
            loop {
                self.skip_separators();
                if matches!(self.kind(), TokenKind::RBrace | TokenKind::Eof) {
                    break;
                }
                let before = self.pos;
                let errors_before = self.diagnostics.len();
                let stmt = self.stmt();
                self.ast.push_to(stmt);
                if !self.at_statement_end() {
                    // A statement that already reported an error is not also told it ended
                    // badly; the first message is the useful one.
                    if self.diagnostics.len() == errors_before {
                        self.expected("`;` or a line break after the statement");
                    }
                    self.recover_statement();
                    // A closing bracket here has nothing to close.
                    if matches!(self.kind(), TokenKind::RParen | TokenKind::RBracket) {
                        self.bump();
                    }
                }
                if self.pos == before {
                    self.bump();
                }
            }
            self.no_struct = saved;
            self.expect(TokenKind::RBrace, "`}`");
        }
        let stmts = self.ast.finish(mark);
        let span = start.to(self.prev_span());
        self.ast.push_block(Block { stmts, span })
    }

    fn stmt(&mut self) -> StmtId {
        use TokenKind::*;
        let start = self.span();
        let kind = match self.kind() {
            Let => {
                self.bump();
                let mutable = self.eat(Mut);
                let pat = self.pat();
                let ty = self.eat(Colon).then(|| self.ty());
                let init = if self.expect(Eq, "`=` and a value") {
                    self.expr()
                } else {
                    self.error_expr(self.span())
                };
                StmtKind::Let {
                    mutable,
                    pat,
                    ty,
                    init,
                }
            }
            Return => {
                self.bump();
                let value = (!self.at_statement_end()).then(|| self.expr());
                StmtKind::Return(value)
            }
            Break => {
                self.bump();
                StmtKind::Break
            }
            Continue => {
                self.bump();
                StmtKind::Continue
            }
            Assert => {
                self.bump();
                StmtKind::Assert(self.expr())
            }
            _ => {
                let expr = self.expr();
                if let Some(op) = assign_op(self.kind()) {
                    self.bump();
                    let value = self.expr();
                    StmtKind::Assign {
                        op,
                        place: expr,
                        value,
                    }
                } else {
                    StmtKind::Expr {
                        expr,
                        semi: self.at(Semi),
                    }
                }
            }
        };
        let span = start.to(self.prev_span());
        self.ast.push_stmt(Stmt { kind, span })
    }
}

// Expressions.
impl Parser<'_> {
    fn push(&mut self, kind: ExprKind, start: Span) -> ExprId {
        let span = start.to(self.prev_span());
        self.ast.push_expr(Expr { kind, span })
    }

    fn span_of(&self, expr: ExprId) -> Span {
        self.ast.expr(expr).span
    }

    fn binary_node(&mut self, op: BinaryOp, lhs: ExprId, rhs: ExprId) -> ExprId {
        let span = self.span_of(lhs).to(self.span_of(rhs));
        self.ast.push_expr(Expr {
            kind: ExprKind::Binary { op, lhs, rhs },
            span,
        })
    }

    fn expr(&mut self) -> ExprId {
        self.or_expr()
    }

    /// An expression that cannot be a struct literal at its top level, so that the `{` after
    /// `if x` starts the body.
    fn expr_no_struct(&mut self) -> ExprId {
        let saved = std::mem::replace(&mut self.no_struct, true);
        let expr = self.expr();
        self.no_struct = saved;
        expr
    }

    fn or_expr(&mut self) -> ExprId {
        let mut lhs = self.and_expr();
        while matches!(self.kind(), TokenKind::Or | TokenKind::OrOr) {
            if self.at(TokenKind::OrOr) {
                let span = self.span();
                self.error(span, "Noev writes logical or as `or`")
                    .with_help_mut("replace `||` with `or`");
            }
            self.bump();
            let rhs = self.and_expr();
            lhs = self.binary_node(BinaryOp::Or, lhs, rhs);
        }
        lhs
    }

    fn and_expr(&mut self) -> ExprId {
        let mut lhs = self.not_expr();
        while matches!(self.kind(), TokenKind::And | TokenKind::AndAnd) {
            if self.at(TokenKind::AndAnd) {
                let span = self.span();
                self.error(span, "Noev writes logical and as `and`")
                    .with_help_mut("replace `&&` with `and`");
            }
            self.bump();
            let rhs = self.not_expr();
            lhs = self.binary_node(BinaryOp::And, lhs, rhs);
        }
        lhs
    }

    fn not_expr(&mut self) -> ExprId {
        if matches!(self.kind(), TokenKind::Not | TokenKind::Bang) {
            let start = self.span();
            if self.at(TokenKind::Bang) {
                self.error(start, "Noev writes logical not as `not`")
                    .with_help_mut("replace `!` with `not`");
            }
            self.bump();
            let operand = self.not_expr();
            return self.push(
                ExprKind::Unary {
                    op: UnaryOp::Not,
                    operand,
                },
                start,
            );
        }
        self.comparison()
    }

    fn comparison(&mut self) -> ExprId {
        let lhs = self.binary(PREC_BIT_OR);
        let Some(op) = comparison_op(self.kind()) else {
            return lhs;
        };
        self.bump();
        self.skip_newlines();
        let rhs = self.binary(PREC_BIT_OR);
        let mut expr = self.binary_node(op, lhs, rhs);
        while let Some(op) = comparison_op(self.kind()) {
            let span = self.span();
            self.error(span, "comparisons cannot be chained")
                .with_help_mut("combine them with `and`, as in `a < b and b < c`");
            self.bump();
            self.skip_newlines();
            let rhs = self.binary(PREC_BIT_OR);
            expr = self.binary_node(op, expr, rhs);
        }
        expr
    }

    /// Precedence climbing over the operators in [`binary_op`] that bind at least as tightly
    /// as `min`.
    fn binary(&mut self, min: u8) -> ExprId {
        let mut lhs = self.unary();
        while let Some((op, prec)) = binary_op(self.kind()) {
            if prec < min {
                break;
            }
            self.bump();
            let rhs = self.binary(prec + 1);
            lhs = self.binary_node(op, lhs, rhs);
        }
        lhs
    }

    fn unary(&mut self) -> ExprId {
        use TokenKind::*;
        let start = self.span();
        match self.kind() {
            Minus | Tilde => {
                let op = if self.bump().kind == Minus {
                    UnaryOp::Neg
                } else {
                    UnaryOp::BitNot
                };
                let operand = self.unary();
                self.push(ExprKind::Unary { op, operand }, start)
            }
            Ident if self.nth_is_word(0, "mv") && self.nth_starts_place(1) => self.own(OwnKind::Mv),
            Ref if self.nth_starts_place(1) => self.own(OwnKind::Ref),
            Mut if self.nth_starts_place(1) => self.own(OwnKind::Mut),
            Ident if self.nth_is_word(0, "adj") && self.nth_starts_place(1) => {
                self.bump();
                let call = self.postfix();
                if !matches!(
                    self.ast.expr(call).kind,
                    ExprKind::Call { .. } | ExprKind::MethodCall { .. } | ExprKind::Error
                ) {
                    let span = self.span_of(call);
                    self.error(span, "`adj` applies to a call, such as `adj q.s()`");
                }
                self.push(ExprKind::Adj(call), start)
            }
            Bra => self.bra(),
            Lt => self.expectation(),
            _ => self.postfix(),
        }
    }

    /// `mv place`, `ref place` or `mut place`, followed by any method calls.
    fn own(&mut self, kind: OwnKind) -> ExprId {
        let start = self.span();
        self.bump();
        let place = self.place();
        let own = self.push(ExprKind::Own { kind, place }, start);
        self.method_calls(own)
    }

    /// A place: a name followed by fields, tuple indices and array indices, stopping before
    /// a method call.
    fn place(&mut self) -> ExprId {
        use TokenKind::*;
        let start = self.span();
        let mut place = match self.kind() {
            Ident | SelfValue => {
                self.bump();
                let name = self.intern(start);
                self.push(ExprKind::Name(name), start)
            }
            _ => {
                self.expected("a variable or field");
                return self.error_expr(start);
            }
        };
        loop {
            match (self.kind(), self.nth(1).kind, self.nth(2).kind) {
                (Dot, Ident, next) if next != LParen => {
                    self.bump();
                    let span = self.bump().span;
                    let name = self.intern(span);
                    place = self.push(ExprKind::Field { base: place, name }, start);
                }
                (Dot, Int, _) => {
                    self.bump();
                    let index = self.tuple_index();
                    place = self.push(ExprKind::TupleIndex { base: place, index }, start);
                }
                (LBracket, ..) => {
                    let index = self.index();
                    place = self.push(ExprKind::Index { base: place, index }, start);
                }
                _ => return place,
            }
        }
    }

    fn method_calls(&mut self, mut receiver: ExprId) -> ExprId {
        let start = self.span_of(receiver);
        while self.at(TokenKind::Dot)
            && self.nth(1).kind == TokenKind::Ident
            && self.nth(2).kind == TokenKind::LParen
        {
            self.bump();
            let span = self.bump().span;
            let method = self.intern(span);
            let args = self.args();
            receiver = self.push(
                ExprKind::MethodCall {
                    receiver,
                    method,
                    args,
                },
                start,
            );
        }
        receiver
    }

    fn tuple_index(&mut self) -> u32 {
        let span = self.bump().span;
        self.text(span).parse().unwrap_or_else(|_| {
            self.error(span, "tuple index is too large");
            0
        })
    }

    /// `[expr]`.
    fn index(&mut self) -> ExprId {
        self.bump();
        let saved = std::mem::replace(&mut self.no_struct, false);
        let index = self.expr();
        self.no_struct = saved;
        self.expect(TokenKind::RBracket, "`]`");
        index
    }

    /// `(args)`, starting at `(`.
    fn args(&mut self) -> List<ExprId> {
        self.bump();
        let saved = std::mem::replace(&mut self.no_struct, false);
        let mark = self.ast.mark::<ExprId>();
        while !matches!(
            self.kind(),
            TokenKind::RParen | TokenKind::RBrace | TokenKind::Eof
        ) {
            let arg = self.expr();
            self.ast.push_to(arg);
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.no_struct = saved;
        self.expect(TokenKind::RParen, "`)` or `,`");
        self.ast.finish(mark)
    }

    fn postfix(&mut self) -> ExprId {
        use TokenKind::*;
        let mut expr = self.primary();
        let start = self.span_of(expr);
        loop {
            match self.kind() {
                Dot => {
                    self.bump();
                    match self.kind() {
                        Ident => {
                            let span = self.bump().span;
                            let name = self.intern(span);
                            expr = if self.at(LParen) {
                                let args = self.args();
                                self.push(
                                    ExprKind::MethodCall {
                                        receiver: expr,
                                        method: name,
                                        args,
                                    },
                                    start,
                                )
                            } else {
                                self.push(ExprKind::Field { base: expr, name }, start)
                            };
                        }
                        Int => {
                            let index = self.tuple_index();
                            expr = self.push(ExprKind::TupleIndex { base: expr, index }, start);
                        }
                        _ => {
                            self.expected("a field, method or tuple index after `.`");
                            return expr;
                        }
                    }
                }
                LBracket => {
                    let index = self.index();
                    expr = self.push(ExprKind::Index { base: expr, index }, start);
                }
                LParen => {
                    let args = self.args();
                    expr = self.push(ExprKind::Call { callee: expr, args }, start);
                }
                _ => return expr,
            }
        }
    }

    fn primary(&mut self) -> ExprId {
        use TokenKind::*;
        let start = self.span();
        let kind = match self.kind() {
            Int => {
                self.bump();
                ExprKind::Int(self.int_value(start).unwrap_or(0))
            }
            Float => {
                self.bump();
                let text = self.text(start).replace('_', "");
                ExprKind::Float(text.parse().unwrap_or(0.0))
            }
            True | False => ExprKind::Bool(self.bump().kind == True),
            Ket => {
                self.bump();
                ExprKind::Ket
            }
            Ident if self.nth_is_word(0, "ctrl") && self.nth_starts_place(1) => {
                return self.ctrl();
            }
            Ident if self.nth_is_word(0, "with") && self.nth(1).kind == LBrace => {
                return self.with();
            }
            Ident | SelfValue | SelfType => {
                let path = self.path_expr();
                if !self.no_struct && self.at(LBrace) {
                    return self.struct_lit(path);
                }
                return path;
            }
            LParen => return self.paren(),
            LBracket => return self.array(),
            LBrace => ExprKind::Block(self.block()),
            If => return self.if_expr(),
            While => {
                self.bump();
                let cond = self.expr_no_struct();
                self.skip_newlines();
                let body = self.block();
                ExprKind::While { cond, body }
            }
            For => return self.for_expr(),
            Unsafe => {
                self.bump();
                self.skip_newlines();
                ExprKind::Unsafe(self.block())
            }
            _ => {
                self.expected("an expression");
                if !self.at_statement_end() && !matches!(self.kind(), RParen | RBracket) {
                    self.bump();
                }
                return self.error_expr(start);
            }
        };
        self.push(kind, start)
    }

    fn int_value(&mut self, span: Span) -> Option<u64> {
        let text = self.text(span).replace('_', "");
        let value = if let Some(hex) = text.strip_prefix("0x") {
            u64::from_str_radix(hex, 16)
        } else if let Some(bin) = text.strip_prefix("0b") {
            u64::from_str_radix(bin, 2)
        } else {
            text.parse()
        };
        match value {
            Ok(value) => Some(value),
            Err(_) => {
                self.error(span, "integer literal is too large");
                None
            }
        }
    }

    fn path_expr(&mut self) -> ExprId {
        let start = self.span();
        let segments = self.path_segments();
        let kind = match self.ast.list(segments) {
            [single] => ExprKind::Name(*single),
            _ => ExprKind::Path(segments),
        };
        self.push(kind, start)
    }

    fn struct_lit(&mut self, path: ExprId) -> ExprId {
        let start = self.span_of(path);
        self.bump();
        let mark = self.ast.mark::<FieldInit>();
        let saved = std::mem::replace(&mut self.no_struct, false);
        loop {
            while matches!(
                self.kind(),
                TokenKind::Newline | TokenKind::Comma | TokenKind::Semi
            ) {
                self.bump();
            }
            if matches!(self.kind(), TokenKind::RBrace | TokenKind::Eof) {
                break;
            }
            let field_start = self.span();
            if !self.at(TokenKind::Ident) {
                self.expected("a field name");
                self.recover_member();
                continue;
            }
            let (name, _) = self.name("a field name");
            let value = self.eat(TokenKind::Colon).then(|| self.expr());
            let span = field_start.to(self.prev_span());
            self.ast.push_to(FieldInit { name, value, span });
        }
        self.no_struct = saved;
        self.expect(TokenKind::RBrace, "`}`");
        let fields = self.ast.finish(mark);
        self.push(ExprKind::StructLit { path, fields }, start)
    }

    /// `()`, `(expr)` or a tuple.
    fn paren(&mut self) -> ExprId {
        let start = self.span();
        self.bump();
        if self.eat(TokenKind::RParen) {
            return self.push(ExprKind::Unit, start);
        }
        let saved = std::mem::replace(&mut self.no_struct, false);
        let first = self.expr();
        let result = if self.at(TokenKind::Comma) {
            let mark = self.ast.mark::<ExprId>();
            self.ast.push_to(first);
            while self.eat(TokenKind::Comma) && !self.at(TokenKind::RParen) {
                let element = self.expr();
                self.ast.push_to(element);
            }
            self.expect(TokenKind::RParen, "`)` or `,`");
            let elements = self.ast.finish(mark);
            self.push(ExprKind::Tuple(elements), start)
        } else {
            self.expect(TokenKind::RParen, "`)`");
            first
        };
        self.no_struct = saved;
        result
    }

    /// `[a, b]` or `[value; count]`.
    fn array(&mut self) -> ExprId {
        let start = self.span();
        self.bump();
        let saved = std::mem::replace(&mut self.no_struct, false);
        let result = if self.eat(TokenKind::RBracket) {
            self.push(ExprKind::Array(List::EMPTY), start)
        } else {
            let first = self.expr();
            if self.eat(TokenKind::Semi) {
                let count = self.expr();
                self.expect(TokenKind::RBracket, "`]`");
                self.push(
                    ExprKind::ArrayRepeat {
                        value: first,
                        count,
                    },
                    start,
                )
            } else {
                let mark = self.ast.mark::<ExprId>();
                self.ast.push_to(first);
                while self.eat(TokenKind::Comma) && !self.at(TokenKind::RBracket) {
                    let element = self.expr();
                    self.ast.push_to(element);
                }
                self.expect(TokenKind::RBracket, "`]` or `,`");
                let elements = self.ast.finish(mark);
                self.push(ExprKind::Array(elements), start)
            }
        };
        self.no_struct = saved;
        result
    }

    fn if_expr(&mut self) -> ExprId {
        let start = self.span();
        self.bump();
        let cond = self.expr_no_struct();
        self.skip_newlines();
        let then = self.block();
        let otherwise = self.eat(TokenKind::Else).then(|| {
            if self.at(TokenKind::If) {
                self.if_expr()
            } else {
                let block_start = self.span();
                let block = self.block();
                self.push(ExprKind::Block(block), block_start)
            }
        });
        self.push(
            ExprKind::If {
                cond,
                then,
                otherwise,
            },
            start,
        )
    }

    fn for_expr(&mut self) -> ExprId {
        let start = self.span();
        self.bump();
        let (var, _) = self.name("a loop variable");
        self.expect(TokenKind::In, "`in`");
        let range_start = self.expr_no_struct();
        self.expect(TokenKind::DotDot, "`..` and the end of the range");
        let range_end = self.expr_no_struct();
        self.skip_newlines();
        let body = self.block();
        self.push(
            ExprKind::For {
                var,
                start: range_start,
                end: range_end,
                body,
            },
            start,
        )
    }

    fn ctrl(&mut self) -> ExprId {
        let start = self.span();
        self.bump();
        let mark = self.ast.mark::<ExprId>();
        loop {
            let control = self.place();
            self.ast.push_to(control);
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        let controls = self.ast.finish(mark);
        self.skip_newlines();
        let body = self.block();
        self.push(ExprKind::Ctrl { controls, body }, start)
    }

    fn with(&mut self) -> ExprId {
        let start = self.span();
        self.bump();
        let with = self.block();
        let body = if self.nth_is_word(0, "do") {
            self.bump();
            self.skip_newlines();
            self.block()
        } else {
            self.expected("`do` and the block to run before undoing `with`");
            with
        };
        self.push(ExprKind::With { with, body }, start)
    }

    /// After a bra: a measurement test `<s| operand`, or an expectation value
    /// `<s| observable |s>`.
    fn bra(&mut self) -> ExprId {
        let bra = self.bump().span;
        let operand = self.binary(PREC_BIT_XOR);
        if !self.at(TokenKind::Pipe) {
            if let ExprKind::Binary { op, .. } = self.ast.expr(operand).kind {
                let span = self.span_of(operand);
                self.error(span, "a measurement test applies to a single value")
                    .with_help_mut(format!(
                        "wrap it in parentheses if `{}` is meant to be measured",
                        op.as_str()
                    ));
            }
            return self.push(
                ExprKind::BraTest {
                    state: bra,
                    operand,
                },
                bra,
            );
        }
        let ket = self.closing_ket();
        let source = self.source;
        let bra_state = &source[bra.start as usize + 1..bra.end as usize - 1];
        if let Some(ket) = ket {
            let ket_state = &source[ket.start as usize + 1..ket.end as usize - 1];
            if bra_state != ket_state {
                self.error(
                    ket,
                    "both sides of an expectation value must be the same state",
                )
                .with_label_mut(bra, "the bra")
                .with_help_mut(format!("write `|{bra_state}>`"));
            }
        }
        let prep_span = ket.unwrap_or(bra);
        let prep = self.ast.push_expr(Expr {
            kind: ExprKind::Ket,
            span: prep_span,
        });
        self.push(
            ExprKind::Expectation {
                prep,
                observable: operand,
            },
            bra,
        )
    }

    /// The ket closing an expectation value, starting at `|`. After the observable, the
    /// lexer has split it into `|` and separate tokens (it follows an operand), so it is read
    /// from the source text and the tokens it covers are skipped.
    fn closing_ket(&mut self) -> Option<Span> {
        let pipe = self.bump().span;
        let bytes = self.source.as_bytes();
        let mut end = pipe.end as usize;
        while matches!(bytes.get(end), Some(b'0' | b'1' | b'+' | b'-')) {
            end += 1;
        }
        if end == pipe.end as usize || bytes.get(end) != Some(&b'>') {
            self.error(
                pipe,
                "expected the closing ket, such as `|0>`, after the observable",
            );
            return None;
        }
        let end = end as u32 + 1;
        while self.span().start < end && !self.at(TokenKind::Eof) {
            self.bump();
        }
        if self.prev_span().end != end {
            self.error(Span::new(pipe.start, end), "expected the closing ket");
        }
        Some(Span::new(pipe.start, end))
    }

    /// `<prep| observable |prep>`, starting at `<`.
    fn expectation(&mut self) -> ExprId {
        let start = self.bump().span;
        let bra_tokens = self.pos;
        let prep = self.postfix();
        let bra_end = self.pos;
        self.expect(TokenKind::Pipe, "`|` after the preparation");
        let observable = self.binary(PREC_BIT_XOR);
        self.expect(TokenKind::Pipe, "`|` and the preparation again");
        let ket_tokens = self.pos;
        let ket = self.postfix();
        let ket_end = self.pos;
        self.expect(TokenKind::Gt, "`>` to close the expectation value");

        let same = bra_end - bra_tokens == ket_end - ket_tokens
            && (0..bra_end - bra_tokens).all(|i| {
                let (a, b) = (self.tokens[bra_tokens + i], self.tokens[ket_tokens + i]);
                a.kind == b.kind && self.text(a.span) == self.text(b.span)
            });
        if !same {
            let (bra_span, ket_span) = (self.span_of(prep), self.span_of(ket));
            self.error(
                ket_span,
                "both sides of an expectation value must be the same preparation",
            )
            .with_label_mut(bra_span, "the bra's preparation");
        }
        self.push(ExprKind::Expectation { prep, observable }, start)
    }
}

/// In-place builders, for adding to a diagnostic after it has been pushed.
trait DiagnosticExt {
    fn with_help_mut(&mut self, help: impl Into<String>) -> &mut Self;
    fn with_label_mut(&mut self, span: Span, message: impl Into<String>) -> &mut Self;
}

impl DiagnosticExt for Diagnostic {
    fn with_help_mut(&mut self, help: impl Into<String>) -> &mut Self {
        self.help = Some(help.into());
        self
    }

    fn with_label_mut(&mut self, span: Span, message: impl Into<String>) -> &mut Self {
        self.labels.push((span, message.into()));
        self
    }
}

#[cfg(test)]
mod tests;
