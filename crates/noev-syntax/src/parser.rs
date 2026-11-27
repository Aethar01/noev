//! Turns tokens into a syntax tree.
//!
//! A hand-written recursive-descent parser, with precedence climbing for binary operators.
//! It never backtracks: every decision is made from the current token and at most two more.
//! After an error it records a diagnostic, produces an error node, and skips to the end of
//! the statement or item, so one mistake does not hide the rest of the file.

use crate::ast::*;
use crate::diagnostic::Diagnostic;
use crate::lexer::{lex_at, scan_interpolation, scan_string};
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

/// Lexes and parses `source`, a file whose offsets start at 0.
pub fn parse(source: &str) -> Parsed {
    parse_at(source, 0)
}

/// Lexes and parses `source`, a file whose offsets start at `base` ([`crate::SourceMap`]), as
/// a package with one module.
pub fn parse_at(source: &str, base: u32) -> Parsed {
    let mut ast = Ast::new();
    let package = ast.add_package("main");
    let root = ast.package(package).root;
    let diagnostics = parse_module(&mut ast, root, source, base);
    ast.collect_root();
    Parsed { ast, diagnostics }
}

/// Lexes and parses `source`, a file whose offsets start at `base`, as the items of `module`.
/// Once every module is parsed, [`Ast::collect_root`] gathers their items.
pub fn parse_module(ast: &mut Ast, module: ModuleId, source: &str, base: u32) -> Vec<Diagnostic> {
    let lexed = lex_at(source, base);
    let mut parser = Parser {
        source,
        base,
        tokens: lexed.tokens,
        pos: 0,
        ast: std::mem::take(ast),
        diagnostics: lexed.diagnostics,
        no_struct: false,
    };
    let items = parser.program();
    *ast = parser.ast;
    ast.set_items(module, items);
    parser.diagnostics
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
    /// The offset of `source`'s first byte: spans are offsets, and `source` is indexed by
    /// positions, which [`Parser::pos_of`] and [`Parser::offset_of`] convert between.
    base: u32,
    tokens: Vec<Token>,
    pos: usize,
    ast: Ast,
    diagnostics: Vec<Diagnostic>,
    /// Whether a struct literal is disallowed here, as in `if x { … }`.
    no_struct: bool,
}

// Token access.
impl<'a> Parser<'a> {
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

    fn text(&self, span: Span) -> &'a str {
        &self.source[self.pos_of(span.start)..self.pos_of(span.end)]
    }

    /// The position in `source` of the offset `offset`.
    fn pos_of(&self, offset: u32) -> usize {
        (offset - self.base) as usize
    }

    /// The offset of the position `index` in `source`.
    fn offset_of(&self, index: usize) -> u32 {
        index as u32 + self.base
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
        let text = self.text(span);
        self.ast.symbols.intern(text)
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
                Fn | Struct | Trait | Impl | Unsafe | Extern | Pub | Use if depth == 0 => return,
                Ident if depth == 0 && self.at_fn_start() => return,
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
    fn program(&mut self) -> List<ItemId> {
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
        self.ast.finish(mark)
    }

    fn at_fn_start(&self) -> bool {
        use TokenKind::*;
        match self.kind() {
            Fn | Unsafe => true,
            Ident => {
                (self.nth_is_word(0, "rev")
                    && (self.nth(1).kind == Fn
                        || (self.nth_is_word(1, "circuit") && self.nth(2).kind == Fn)))
                    || (self.nth_is_word(0, "circuit") && self.nth(1).kind == Fn)
            }
            _ => false,
        }
    }

    /// `pub`, `pub(pkg)`, `pub(super)`, `pub(self)` or `pub(in path)`, if there is one
    /// (`spec/modules.md` §4). After `pub`, a `(` always starts a visibility.
    fn vis(&mut self) -> Vis {
        if !self.eat(TokenKind::Pub) {
            return Vis::Private;
        }
        if !self.eat(TokenKind::LParen) {
            return Vis::Pub;
        }
        let vis = if self.eat(TokenKind::SelfValue) {
            Vis::Private
        } else if self.nth_is_word(0, "pkg") {
            self.bump();
            Vis::Pkg
        } else if self.nth_is_word(0, "super") {
            self.bump();
            Vis::Super
        } else if self.eat(TokenKind::In) {
            let start = self.span();
            let path = self.path_segments();
            Vis::In {
                path,
                span: start.to(self.prev_span()),
            }
        } else {
            self.expected("`pkg`, `super`, `self` or `in` and a module's path");
            while !matches!(self.kind(), TokenKind::RParen | TokenKind::Eof) {
                self.bump();
            }
            Vis::Pub
        };
        self.expect(TokenKind::RParen, "`)`");
        vis
    }

    /// Parses a visibility where none is allowed, and reports it.
    fn no_vis(&mut self, what: &str) {
        let start = self.span();
        self.vis();
        if self.span() != start {
            let span = start.to(self.prev_span());
            let message = format!("{what} cannot be marked `pub`");
            self.error(span, message);
        }
    }

    fn item(&mut self) -> Option<ItemId> {
        let start = self.span();
        let vis = self.vis();
        if self.at(TokenKind::Impl) && self.span() != start {
            let span = start.to(self.prev_span());
            self.error(span, "an `impl` block cannot be marked `pub`")
                .with_help_mut("mark its functions instead");
        }
        if self.at(TokenKind::Use) {
            let imports = self.use_decl();
            return Some(self.ast.push_item(Item {
                kind: ItemKind::Use(imports),
                vis,
                span: start.to(self.prev_span()),
            }));
        }
        let kind = match self.kind() {
            _ if self.at_fn_start() => ItemKind::Fn(self.fn_decl(true, vis)),
            TokenKind::Struct => self.struct_def(),
            TokenKind::Enum => self.enum_def(),
            TokenKind::Type => {
                self.bump();
                let (name, name_span) = self.name("a type name");
                self.expect(TokenKind::Eq, "`=`");
                let ty = self.ty();
                ItemKind::Type {
                    name,
                    name_span,
                    ty,
                }
            }
            TokenKind::Trait => self.trait_def(),
            TokenKind::Impl => self.impl_def(),
            TokenKind::Extern => {
                let span = self.span();
                self.error(span, "`extern` functions are not supported yet")
                    .with_help_mut("see spec/interop.md for what is planned");
                self.bump();
                // Read the declaration so it is not reported again as a broken item.
                if self.at_fn_start() {
                    self.fn_decl(false, vis);
                } else {
                    self.recover_item();
                }
                return Some(self.ast.push_item(Item {
                    kind: ItemKind::Error,
                    vis,
                    span: start.to(self.prev_span()),
                }));
            }
            _ => return None,
        };
        let span = start.to(self.prev_span());
        Some(self.ast.push_item(Item { kind, vis, span }))
    }

    /// `use tree`, with the tree's braces flattened into one import per name.
    fn use_decl(&mut self) -> List<Import> {
        self.bump();
        let mark = self.ast.mark::<Import>();
        let mut prefix = Vec::new();
        self.use_tree(&mut prefix, false);
        self.ast.finish(mark)
    }

    /// One tree of a `use`, after `prefix`: `a::b`, `a::b as c`, `a::*`, `a::{…}`, or `self`
    /// directly inside braces.
    fn use_tree(&mut self, prefix: &mut Vec<Symbol>, in_braces: bool) {
        use TokenKind::*;
        let start = self.span();
        let depth = prefix.len();
        loop {
            let span = self.span();
            match self.kind() {
                SelfValue if in_braces && prefix.len() == depth => {
                    // `a::{self}` imports the module `a` itself.
                    self.bump();
                    let alias = self.use_alias();
                    self.push_import(prefix, alias, false, start);
                    break;
                }
                Ident => {
                    self.bump();
                    let symbol = self.intern(span);
                    prefix.push(symbol);
                }
                _ => {
                    self.expected("a name to import");
                    self.recover_use();
                    break;
                }
            }
            if !self.eat(ColonColon) {
                let alias = self.use_alias();
                self.push_import(prefix, alias, false, start);
                break;
            }
            if self.eat(Star) {
                self.push_import(prefix, None, true, start);
                break;
            }
            if self.eat(LBrace) {
                loop {
                    self.skip_newlines();
                    if matches!(self.kind(), RBrace | Eof) {
                        break;
                    }
                    self.use_tree(prefix, true);
                    self.skip_newlines();
                    if !self.eat(Comma) {
                        break;
                    }
                }
                self.skip_newlines();
                self.expect(RBrace, "`}` or `,`");
                break;
            }
        }
        prefix.truncate(depth);
    }

    /// `as name` or `as _` after an imported name.
    fn use_alias(&mut self) -> Option<Symbol> {
        if !self.eat(TokenKind::As) {
            return None;
        }
        let (name, _) = self.name("a name to import it as");
        Some(name)
    }

    fn push_import(&mut self, path: &[Symbol], alias: Option<Symbol>, glob: bool, start: Span) {
        let mark = self.ast.mark::<Symbol>();
        for &segment in path {
            self.ast.push_to(segment);
        }
        let path = self.ast.finish(mark);
        let span = start.to(self.prev_span());
        self.ast.push_to(Import {
            path,
            alias,
            glob,
            span,
        });
    }

    /// Skips the rest of a broken `use` tree, up to a `,` or `}` of its braces or the end of
    /// the statement.
    fn recover_use(&mut self) {
        use TokenKind::*;
        while !matches!(self.kind(), Comma | RBrace | Newline | Semi | Eof) {
            self.bump();
        }
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

    /// `[unsafe] [rev] [circuit] fn name(params) [-> type] block`, or without the block when
    /// `needs_body` is false (trait declarations), visible as `vis` says.
    fn fn_decl(&mut self, needs_body: bool, vis: Vis) -> FnId {
        let start = self.span();
        let is_unsafe = self.eat(TokenKind::Unsafe);
        let is_rev = self.nth_is_word(0, "rev");
        if is_rev {
            self.bump();
        }
        let is_circuit = self.nth_is_word(0, "circuit") && self.nth(1).kind == TokenKind::Fn;
        if is_circuit {
            self.bump();
        }
        self.expect(TokenKind::Fn, "`fn`");
        let (name, name_span) = self.name("a function name");
        let mut generics = self.generics();

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
        generics.wheres = self.where_clause();
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
            generics,
            vis,
            is_unsafe,
            is_rev,
            is_circuit,
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
        let generics = self.generics();
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
                let start = self.span();
                let vis = self.vis();
                if self.at_fn_start() {
                    let method = self.fn_decl(true, vis);
                    self.ast.push_to(method);
                } else if self.at(TokenKind::Ident) {
                    let (name, _) = self.name("a field name");
                    self.expect(TokenKind::Colon, "`:` and the field's type");
                    let ty = self.ty();
                    let span = start.to(self.prev_span());
                    self.ast.push_to(FieldDecl {
                        name,
                        vis,
                        ty,
                        span,
                    });
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
            generics,
            fields: self.ast.finish(field_mark),
            methods: self.ast.finish(method_mark),
        }
    }

    fn enum_def(&mut self) -> ItemKind {
        self.bump();
        let (name, name_span) = self.name("an enum name");
        let generics = self.generics();
        let variant_mark = self.ast.mark::<VariantDecl>();
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
                let start = self.span();
                let vis = self.vis();
                if self.at_fn_start() {
                    let method = self.fn_decl(true, vis);
                    self.ast.push_to(method);
                } else if self.at(TokenKind::Ident) {
                    if self.span() != start {
                        let span = start.to(self.prev_span());
                        self.error(span, "a variant cannot be marked `pub`")
                            .with_help_mut("a variant is as visible as its enum");
                    }
                    let variant = self.variant();
                    self.ast.push_to(variant);
                } else {
                    self.expected("a variant or `fn`");
                    self.recover_member();
                }
            }
            self.expect(TokenKind::RBrace, "`}`");
        }
        ItemKind::Enum {
            name,
            name_span,
            generics,
            variants: self.ast.finish(variant_mark),
            methods: self.ast.finish(method_mark),
        }
    }

    /// `Name`, `Name(T, …)` or `Name { field: T, … }`.
    fn variant(&mut self) -> VariantDecl {
        let start = self.span();
        let (name, _) = self.name("a variant name");
        let data = if self.eat(TokenKind::LParen) {
            let mark = self.ast.mark::<TypeId>();
            while !matches!(self.kind(), TokenKind::RParen | TokenKind::Eof) {
                let ty = self.ty();
                self.ast.push_to(ty);
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
            self.expect(TokenKind::RParen, "`)` or `,`");
            VariantData::Tuple(self.ast.finish(mark))
        } else if self.at(TokenKind::LBrace) {
            self.bump();
            let mark = self.ast.mark::<FieldDecl>();
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
                let field_start = self.span();
                self.no_vis("a variant's field");
                let (name, _) = self.name("a field name");
                self.expect(TokenKind::Colon, "`:` and the field's type");
                let ty = self.ty();
                let span = field_start.to(self.prev_span());
                self.ast.push_to(FieldDecl {
                    name,
                    vis: Vis::Private,
                    ty,
                    span,
                });
            }
            self.expect(TokenKind::RBrace, "`}`");
            VariantData::Fields(self.ast.finish(mark))
        } else {
            VariantData::Unit
        };
        VariantDecl {
            name,
            data,
            span: start.to(self.prev_span()),
        }
    }

    fn trait_def(&mut self) -> ItemKind {
        self.bump();
        let (name, name_span) = self.name("a trait name");
        let generics = self.generics();
        let fn_mark = self.ast.mark::<FnId>();
        let type_mark = self.ast.mark::<AssocType>();
        self.skip_newlines();
        if self.expect(TokenKind::LBrace, "`{`") {
            loop {
                self.skip_separators();
                if matches!(self.kind(), TokenKind::RBrace | TokenKind::Eof) {
                    break;
                }
                if self.at_fn_start() {
                    let f = self.fn_decl(false, Vis::Private);
                    self.ast.push_to(f);
                } else if self.eat(TokenKind::Type) {
                    let (name, _) = self.name("an associated type name");
                    let bounds = if self.eat(TokenKind::Colon) {
                        self.bounds()
                    } else {
                        List::default()
                    };
                    self.ast.push_to(AssocType { name, bounds });
                } else {
                    self.expected("`fn` or `type`");
                    self.recover_member();
                }
            }
            self.expect(TokenKind::RBrace, "`}`");
        }
        ItemKind::Trait {
            name,
            name_span,
            generics,
            fns: self.ast.finish(fn_mark),
            types: self.ast.finish(type_mark),
        }
    }

    fn impl_def(&mut self) -> ItemKind {
        self.bump();
        let mut generics = self.generics();
        let bound = self.bound();
        self.expect(TokenKind::For, "`for`");
        let ty = self.ty();
        generics.wheres = self.where_clause();
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
                    let f = self.fn_decl(true, Vis::Private);
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
            generics,
            bound,
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
            // `swap::<Int>`: the arguments are not a segment.
            if self.at(TokenKind::ColonColon) && self.nth(1).kind == TokenKind::Lt {
                break;
            }
            if !self.eat(TokenKind::ColonColon) {
                break;
            }
        }
        self.ast.finish(mark)
    }

    /// `<T: Bound, const N: Int>` after a name, if there is one.
    fn generics(&mut self) -> Generics {
        use TokenKind::*;
        if !self.eat(Lt) {
            return Generics::default();
        }
        let mark = self.ast.mark::<GenericParam>();
        while !matches!(self.kind(), Gt | Shr | Eof) {
            let start = self.span();
            let param = if self.nth_is_word(0, "const") && self.nth(1).kind == Ident {
                self.bump();
                let (name, _) = self.name("a size parameter's name");
                self.expect(Colon, "`:` and the size's type, `Int`");
                let ty = self.ty();
                let default = if self.eat(Eq) {
                    Some(self.type_arg())
                } else {
                    None
                };
                GenericParam {
                    name,
                    kind: GenericKind::Size { ty, default },
                    span: start.to(self.prev_span()),
                }
            } else {
                let (name, _) = self.name("a type parameter's name");
                let bounds = if self.eat(Colon) {
                    self.bounds()
                } else {
                    List::default()
                };
                let default = if self.eat(Eq) { Some(self.ty()) } else { None };
                GenericParam {
                    name,
                    kind: GenericKind::Type { bounds, default },
                    span: start.to(self.prev_span()),
                }
            };
            self.ast.push_to(param);
            if !self.eat(Comma) {
                break;
            }
        }
        self.close_angle();
        Generics {
            params: self.ast.finish(mark),
            wheres: List::default(),
        }
    }

    /// `Bound + Bound + …`.
    fn bounds(&mut self) -> List<Bound> {
        let mark = self.ast.mark::<Bound>();
        loop {
            let bound = self.bound();
            self.ast.push_to(bound);
            if !self.eat(TokenKind::Plus) {
                break;
            }
        }
        self.ast.finish(mark)
    }

    /// A trait and its arguments: `Measure`, `Oracle<4>`, `Measure<Result = Bit>`.
    fn bound(&mut self) -> Bound {
        use TokenKind::*;
        let start = self.span();
        let path = self.path_segments();
        let mark = self.ast.mark::<BoundArg>();
        if self.eat(Lt) {
            while !matches!(self.kind(), Gt | Shr | Eof) {
                let arg = if self.at(Ident) && self.nth(1).kind == Eq {
                    let (name, _) = self.name("an associated type's name");
                    self.bump();
                    BoundArg::Assoc {
                        name,
                        ty: self.ty(),
                    }
                } else {
                    BoundArg::Type(self.type_arg())
                };
                self.ast.push_to(arg);
                if !self.eat(Comma) {
                    break;
                }
            }
            self.close_angle();
        }
        Bound {
            path,
            args: self.ast.finish(mark),
            span: start.to(self.prev_span()),
        }
    }

    /// A type, or a size given as a number: the `8` of `Register<8>`.
    fn type_arg(&mut self) -> TypeId {
        let start = self.span();
        if self.at(TokenKind::Int) {
            self.bump();
            let len = self.int_value(start).unwrap_or(0);
            return self.ast.push_type(TypeExpr {
                kind: TypeKind::Size(len),
                span: start,
            });
        }
        // `Reg<{N + 1}>`: a size computed from size parameters, in braces so its `+` is not
        // read as part of the type.
        if self.eat(TokenKind::LBrace) {
            let expr = self.expr();
            self.expect(TokenKind::RBrace, "`}`");
            return self.ast.push_type(TypeExpr {
                kind: TypeKind::SizeExpr(expr),
                span: start.to(self.prev_span()),
            });
        }
        self.ty()
    }

    /// `where T: Bound, U: Bound` after a signature, if there is one, on the same line or
    /// the next.
    fn where_clause(&mut self) -> List<WherePred> {
        use TokenKind::*;
        let next_line = self.at(Newline) && self.nth_is_word(1, "where");
        if !next_line && !self.nth_is_word(0, "where") {
            return List::default();
        }
        if next_line {
            self.bump();
        }
        self.bump();
        let mark = self.ast.mark::<WherePred>();
        loop {
            self.skip_newlines();
            if !self.at(Ident) {
                break;
            }
            let start = self.span();
            let (name, _) = self.name("a parameter's name");
            self.expect(Colon, "`:` and its bounds");
            let bounds = self.bounds();
            self.ast.push_to(WherePred {
                name,
                bounds,
                span: start.to(self.prev_span()),
            });
            if !self.eat(Comma) {
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
                let simple = self.nth(1).kind == RBracket;
                let len = if simple && self.eat(Int) {
                    Len::Int(self.int_value(len_span).unwrap_or(0))
                } else if simple && self.at(Ident) {
                    Len::Param(self.name("a size").0)
                } else if matches!(self.kind(), Int | Ident | LParen) {
                    // `N + 1`, `2 * N`: the checker reads it as a size.
                    Len::Expr(self.expr())
                } else {
                    self.expected("the array's length");
                    Len::Int(0)
                };
                self.expect(RBracket, "`]`");
                TypeKind::Array { elem, len }
            }
            Ident | SelfType => {
                let path = self.path_segments();
                if self.at(Lt) {
                    self.bump();
                    let mark = self.ast.mark::<TypeId>();
                    while !matches!(self.kind(), Gt | Shr | Eof) {
                        let arg = self.type_arg();
                        self.ast.push_to(arg);
                        if !self.eat(Comma) {
                            break;
                        }
                    }
                    self.close_angle();
                    TypeKind::Generic {
                        path,
                        args: self.ast.finish(mark),
                    }
                } else {
                    TypeKind::Path(path)
                }
            }
            _ => {
                self.expected("a type");
                TypeKind::Error
            }
        };
        let span = start.to(self.prev_span());
        self.ast.push_type(TypeExpr { kind, span })
    }

    /// Consumes a `>` closing type arguments. In `A<B<C>>` the lexer sees `>>`, so it is
    /// split: the first `>` is consumed and the second left for the outer list.
    fn close_angle(&mut self) {
        match self.kind() {
            TokenKind::Gt => {
                self.bump();
            }
            TokenKind::Shr => {
                let token = &mut self.tokens[self.pos];
                token.kind = TokenKind::Gt;
                token.span.start += 1;
            }
            _ => {
                self.expected("`>` to close the type arguments");
            }
        }
    }

    /// The data part of a variant pattern: `(p, …)`, `{ field: p, …, .. }`, or nothing.
    fn pat_data(&mut self) -> PatData {
        use TokenKind::*;
        if self.eat(LParen) {
            let mark = self.ast.mark::<PatId>();
            while !matches!(self.kind(), RParen | Eof) {
                let pat = self.pat();
                self.ast.push_to(pat);
                if !self.eat(Comma) {
                    break;
                }
            }
            self.expect(RParen, "`)` or `,`");
            return PatData::Tuple(self.ast.finish(mark));
        }
        if !self.at(LBrace) {
            return PatData::None;
        }
        self.bump();
        let mark = self.ast.mark::<FieldPat>();
        let mut rest = false;
        loop {
            self.skip_newlines();
            if matches!(self.kind(), RBrace | Eof) {
                break;
            }
            if self.eat(DotDot) {
                rest = true;
                self.skip_newlines();
                break;
            }
            let start = self.span();
            let (name, name_span) = self.name("a field name");
            // `field` alone is short for `field: field`.
            let pat = if self.eat(Colon) {
                self.pat()
            } else {
                self.ast.push_pat(Pat {
                    kind: PatKind::Name(name),
                    span: name_span,
                })
            };
            let span = start.to(self.prev_span());
            self.ast.push_to(FieldPat {
                name,
                pat: Some(pat),
                span,
            });
            self.skip_newlines();
            if !self.eat(Comma) {
                break;
            }
        }
        self.skip_newlines();
        self.expect(RBrace, "`}` or `,`");
        PatData::Fields {
            fields: self.ast.finish(mark),
            rest,
        }
    }

    fn pat(&mut self) -> PatId {
        use TokenKind::*;
        let start = self.span();
        let kind = match self.kind() {
            Ident if self.text(start) == "_" => {
                self.bump();
                PatKind::Wildcard
            }
            // A variant: a path, `Name(…)`, or one of `Some`, `None`, `Ok` and `Err`.
            Ident | SelfType
                if self.nth(1).kind == ColonColon
                    || self.nth(1).kind == LParen
                    || crate::ast::BARE_VARIANTS.contains(&self.text(start)) =>
            {
                let path = self.path_segments();
                let data = self.pat_data();
                PatKind::Variant { path, data }
            }
            Ident => {
                self.bump();
                PatKind::Name(self.intern(start))
            }
            Int => {
                self.bump();
                PatKind::Literal(PatLiteral::Int(self.int_value(start).unwrap_or(0) as i64))
            }
            Minus if self.nth(1).kind == Int => {
                self.bump();
                let digits = self.span();
                self.bump();
                let value = self.int_value(digits).unwrap_or(0) as i64;
                PatKind::Literal(PatLiteral::Int(value.wrapping_neg()))
            }
            True | False => PatKind::Literal(PatLiteral::Bool(self.bump().kind == True)),
            Str => {
                self.bump();
                let parts = self.string_parts(start);
                let text: String = self
                    .ast
                    .list(parts)
                    .iter()
                    .map(|part| match part {
                        StrPart::Text(t) => self.ast.name(*t).to_string(),
                        StrPart::Value { .. } => String::new(),
                    })
                    .collect();
                if self
                    .ast
                    .list(parts)
                    .iter()
                    .any(|p| matches!(p, StrPart::Value { .. }))
                {
                    self.error(start, "a string in a pattern cannot interpolate values");
                }
                PatKind::Literal(PatLiteral::Str(self.ast.symbols.intern(&text)))
            }
            LParen | LBracket => {
                let close = if self.bump().kind == LParen {
                    RParen
                } else {
                    RBracket
                };
                let mark = self.ast.mark::<PatId>();
                while !matches!(self.kind(), RParen | RBracket | Eof) {
                    let pat = self.pat();
                    self.ast.push_to(pat);
                    if !self.eat(Comma) {
                        break;
                    }
                }
                if close == RParen {
                    self.expect(RParen, "`)` or `,`");
                    PatKind::Tuple(self.ast.finish(mark))
                } else {
                    self.expect(RBracket, "`]` or `,`");
                    PatKind::Array(self.ast.finish(mark))
                }
            }
            _ => {
                if !self.reserved_word() {
                    self.expected(
                        "a pattern: a variable name, `_`, a variant, a literal, `(` or `[`",
                    );
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
        while self.at(TokenKind::As) {
            let span = self.bump().span;
            let ty = self.ty();
            let ty_span = self.ast.ty(ty).span;
            let target = self.text(ty_span).to_string();
            self.error(span.to(ty_span), "Noev has no `as` casts")
                .with_help_mut(format!(
                    "convert with `{target}(…)`; lossy conversions are methods, such as `x.round()`"
                ));
        }
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
            // An expectation value or amplitude ends with `>`, so `.re()` may follow it.
            Bra | Lt => {
                let expr = if self.at(Bra) {
                    self.bra()
                } else {
                    self.expectation()
                };
                match self.ast.expr(expr).kind {
                    ExprKind::Expectation { .. } | ExprKind::Overlap { .. } => {
                        self.postfix_from(expr)
                    }
                    _ => expr,
                }
            }
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
        let expr = self.primary();
        self.postfix_from(expr)
    }

    /// Field accesses, indexing and calls after `expr`.
    fn postfix_from(&mut self, mut expr: ExprId) -> ExprId {
        use TokenKind::*;
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
                // `Int` has any size (spec/types.md §1); only large ones need their text.
                match parse_u64(self.text(start)) {
                    Some(value) => ExprKind::Int(value),
                    None => ExprKind::BigInt,
                }
            }
            Float => {
                self.bump();
                let text = self.text(start).replace('_', "");
                ExprKind::Float(text.parse().unwrap_or(0.0))
            }
            Imag => {
                self.bump();
                let text = self.text(start).replace('_', "");
                ExprKind::Imag(text.trim_end_matches('i').parse().unwrap_or(0.0))
            }
            True | False => ExprKind::Bool(self.bump().kind == True),
            Ket => {
                self.bump();
                let parts = self.ket_parts(start);
                let text = self.text(start);
                let inner = &text[1..text.len().saturating_sub(1)];
                let zero =
                    parts.is_empty() && !inner.is_empty() && inner.bytes().all(|b| b == b'0');
                ExprKind::Ket {
                    qubits: self.qubits(start),
                    zero,
                    parts,
                }
            }
            Str => {
                self.bump();
                ExprKind::Str(self.string_parts(start))
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
            Match => return self.match_expr(),
            Sample => {
                self.bump();
                let shots = self.expr_no_struct();
                self.skip_newlines();
                let body = self.block();
                ExprKind::Sample { shots, body }
            }
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

    /// An integer literal that must fit in a `u64` (an array length, a pattern).
    fn int_value(&mut self, span: Span) -> Option<u64> {
        let value = parse_u64(self.text(span));
        if value.is_none() {
            self.error(span, "integer literal is too large here");
        }
        value
    }

    fn path_expr(&mut self) -> ExprId {
        use TokenKind::*;
        let start = self.span();
        let segments = self.path_segments();
        // `swap::<Int>`: arguments given, not inferred.
        let mut args = None;
        if self.at(ColonColon) && self.nth(1).kind == Lt {
            self.bump();
            self.bump();
            let mark = self.ast.mark::<TypeId>();
            while !matches!(self.kind(), Gt | Shr | Eof) {
                let arg = self.type_arg();
                self.ast.push_to(arg);
                if !self.eat(Comma) {
                    break;
                }
            }
            self.close_angle();
            args = Some(self.ast.finish(mark));
        }
        let kind = match self.ast.list(segments) {
            [single]
                if args.is_none()
                    && !crate::ast::BARE_VARIANTS.contains(&self.ast.name(*single)) =>
            {
                ExprKind::Name(*single)
            }
            _ => ExprKind::Path(segments),
        };
        let expr = self.push(kind, start);
        if let Some(args) = args {
            self.ast.set_path_args(expr, args);
        }
        expr
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

    fn match_expr(&mut self) -> ExprId {
        let start = self.span();
        self.bump();
        let scrutinee = self.expr_no_struct();
        self.skip_newlines();
        let mark = self.ast.mark::<Arm>();
        let saved = std::mem::replace(&mut self.no_struct, false);
        if self.expect(TokenKind::LBrace, "`{` and the arms") {
            loop {
                while matches!(self.kind(), TokenKind::Newline | TokenKind::Comma) {
                    self.bump();
                }
                if matches!(self.kind(), TokenKind::RBrace | TokenKind::Eof) {
                    break;
                }
                let arm_start = self.span();
                let pat = self.pat();
                self.expect(TokenKind::FatArrow, "`=>` and the arm's value");
                self.skip_newlines();
                let body = self.expr();
                let span = arm_start.to(self.prev_span());
                self.ast.push_to(Arm { pat, body, span });
                if !matches!(
                    self.kind(),
                    TokenKind::Newline | TokenKind::Comma | TokenKind::RBrace
                ) {
                    self.expected("`,`, a line break or `}` after the arm");
                    self.recover_member();
                }
            }
            self.expect(TokenKind::RBrace, "`}`");
        }
        self.no_struct = saved;
        let arms = self.ast.finish(mark);
        self.push(ExprKind::Match { scrutinee, arms }, start)
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
        // `<0|+>`: a ket's labels straight after, so an amplitude.
        if let Some(end) = self.ket_rest(bra.end) {
            let ket_span = Span::new(bra.end - 1, end);
            self.skip_to(end);
            let bra_prep = self.state_prep(bra);
            let ket = self.state_prep(ket_span);
            return self.overlap(bra_prep, ket, None, bra);
        }
        let operand = self.binary(PREC_BIT_XOR);
        // `<0|bell()>`.
        if self.at(TokenKind::Gt) && matches!(self.ast.expr(operand).kind, ExprKind::Call { .. }) {
            self.bump();
            let bra_prep = self.state_prep(bra);
            return self.overlap(bra_prep, operand, None, bra);
        }
        if !self.at(TokenKind::Pipe) {
            if let ExprKind::Binary { op, .. } = self.ast.expr(operand).kind {
                let span = self.span_of(operand);
                self.error(span, "a measurement test applies to a single value")
                    .with_help_mut(format!(
                        "wrap it in parentheses if `{}` is meant to be measured",
                        op.as_str()
                    ));
            }
            let parts = self.ket_parts(bra);
            return self.push(
                ExprKind::BraTest {
                    state: bra,
                    qubits: self.qubits(bra),
                    parts,
                    operand,
                },
                bra,
            );
        }
        // `<0| H |prep()>`: a transition value to a preparation.
        if self.ket_rest(self.span().end).is_none() {
            self.bump();
            let ket = self.postfix();
            self.expect(TokenKind::Gt, "`>` to close the transition value");
            let bra_prep = self.state_prep(bra);
            return self.overlap(bra_prep, ket, Some(operand), bra);
        }
        let ket = self.closing_ket();
        let bra_state = &self.text(bra)[1..bra.len() as usize - 1];
        // `<0| H |1>`: a transition value.
        if let Some(ket) = ket {
            let ket_state = &self.text(ket)[1..ket.len() as usize - 1];
            if bra_state != ket_state {
                let bra_prep = self.state_prep(bra);
                let ket_prep = self.state_prep(ket);
                return self.overlap(bra_prep, ket_prep, Some(operand), bra);
            }
        }
        let prep_span = ket.unwrap_or(bra);
        let prep = self.state_prep(prep_span);
        let precision = self.within();
        self.push(
            ExprKind::Expectation {
                prep,
                observable: operand,
                precision,
            },
            bra,
        )
    }

    /// The labels of a bra or ket at `span` as a preparation, a ket expression; computed
    /// labels are reported, since a preparation is run many times.
    fn state_prep(&mut self, span: Span) -> ExprId {
        let text = self.text(span);
        let kind = if text.contains('{') {
            self.error(span, "a preparation takes only fixed labels for now")
                .with_help_mut("prepare the state in a function, as in `<prep(b)| H |prep(b)>`");
            ExprKind::Error
        } else {
            let inner = &text[1..text.len().saturating_sub(1)];
            let zero = !inner.is_empty() && inner.bytes().all(|b| b == b'0');
            ExprKind::Ket {
                qubits: self.qubits(span),
                zero,
                parts: List::default(),
            }
        };
        self.ast.push_expr(Expr { kind, span })
    }

    /// `<φ|ψ>` or `<φ| H |ψ>` with different sides, and its `within`, starting at `start`.
    fn overlap(
        &mut self,
        bra: ExprId,
        ket: ExprId,
        observable: Option<ExprId>,
        start: Span,
    ) -> ExprId {
        let precision = self.within();
        self.push(
            ExprKind::Overlap {
                bra,
                ket,
                observable,
                precision,
            },
            start,
        )
    }

    /// The end of a ket's labels and `>` starting at offset `start`, if the source has them
    /// there: what closes `<φ|` in `<φ|00>`.
    fn ket_rest(&self, start: u32) -> Option<u32> {
        let bytes = self.source.as_bytes();
        let start = self.pos_of(start);
        let len = crate::state::scan(bytes.get(start..)?);
        (len > 0 && bytes.get(start + len) == Some(&b'>')).then(|| self.offset_of(start + len + 1))
    }

    /// Skips the tokens before byte `end`, which a ket read from the source covers.
    fn skip_to(&mut self, end: u32) {
        while self.span().start < end && !self.at(TokenKind::Eof) {
            self.bump();
        }
    }

    /// `within ε` after an expectation value.
    fn within(&mut self) -> Option<ExprId> {
        if !self.nth_is_word(0, "within") {
            return None;
        }
        self.bump();
        Some(self.unary())
    }

    /// How many fixed labels the ket or bra literal at `span` has.
    fn qubits(&self, span: Span) -> u32 {
        let text = self.text(span);
        let mut count = 0;
        let mut bytes = &text.as_bytes()[1..text.len() - 1];
        while !bytes.is_empty() {
            if let Some((_, len)) = crate::state::label(bytes) {
                count += 1;
                bytes = &bytes[len..];
            } else {
                let close = crate::lexer::scan_interpolation(bytes, 0).unwrap_or(bytes.len());
                bytes = &bytes[close..];
            }
        }
        count
    }

    /// The labels of the ket or bra literal at `span` when some are computed, each `{expr}`
    /// parsed from its slice of the source; empty when all are fixed.
    fn ket_parts(&mut self, span: Span) -> List<KetPart> {
        let text = self.text(span);
        if !text.contains('{') {
            return List::default();
        }
        let mark = self.ast.mark::<KetPart>();
        let bytes = self.source.as_bytes();
        let mut pos = self.pos_of(span.start) + 1;
        let end = self.pos_of(span.end) - 1;
        while pos < end {
            if let Some((label, len)) = crate::state::label(&bytes[pos..end]) {
                self.ast.push_to(KetPart::Label(label));
                pos += len;
                continue;
            }
            let close = crate::lexer::scan_interpolation(bytes, pos)
                .unwrap_or(end)
                .min(end);
            let expr = self.sub_expr(self.offset_of(pos) + 1, self.offset_of(close) - 1);
            self.ast.push_to(KetPart::Value(expr));
            pos = close;
        }
        self.ast.finish(mark)
    }

    /// The ket closing an expectation value, starting at `|`. After the observable, the
    /// lexer has split it into `|` and separate tokens (it follows an operand), so it is read
    /// from the source text and the tokens it covers are skipped.
    fn closing_ket(&mut self) -> Option<Span> {
        let pipe = self.bump().span;
        let bytes = self.source.as_bytes();
        let from = self.pos_of(pipe.end);
        let end = from + crate::state::scan(&bytes[from..]);
        if end == from || bytes.get(end) != Some(&b'>') {
            self.error(
                pipe,
                "expected the closing ket, such as `|0>`, after the observable",
            );
            return None;
        }
        let end = self.offset_of(end) + 1;
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
        // `<bell()|00>`: an amplitude, with the ket's labels read from the source.
        if self.at(TokenKind::Pipe)
            && let Some(end) = self.ket_rest(self.span().end)
        {
            let ket_span = Span::new(self.span().start, end);
            self.skip_to(end);
            let ket = self.state_prep(ket_span);
            return self.overlap(prep, ket, None, start);
        }
        self.expect(TokenKind::Pipe, "`|` after the preparation");
        let observable = self.binary(PREC_BIT_XOR);
        // `<φ|ψ>`: an amplitude.
        if self.at(TokenKind::Gt) && matches!(self.ast.expr(observable).kind, ExprKind::Call { .. })
        {
            self.bump();
            return self.overlap(prep, observable, None, start);
        }
        // No closing ket: `<bell()| mv pair`, a measurement test against the preparation.
        if !self.at(TokenKind::Pipe) {
            if let ExprKind::Binary { op, .. } = self.ast.expr(observable).kind {
                let span = self.span_of(observable);
                self.error(span, "a measurement test applies to a single value")
                    .with_help_mut(format!(
                        "wrap it in parentheses if `{}` is meant to be measured",
                        op.as_str()
                    ));
            }
            return self.push(
                ExprKind::PrepTest {
                    prep,
                    operand: observable,
                },
                start,
            );
        }
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
        // Different preparations: a transition value.
        if !same {
            return self.overlap(prep, ket, Some(observable), start);
        }
        let precision = self.within();
        self.push(
            ExprKind::Expectation {
                prep,
                observable,
                precision,
            },
            start,
        )
    }
}

// String literals.
impl Parser<'_> {
    /// Splits a string literal into text and interpolated values, decoding escapes and
    /// parsing each interpolation from its own slice of the source.
    fn string_parts(&mut self, span: Span) -> List<StrPart> {
        let mark = self.ast.mark::<StrPart>();
        let bytes = self.source.as_bytes();
        let start = self.pos_of(span.start) + 1;
        let end = self.pos_of(span.end).saturating_sub(1).max(start);
        let mut text = String::new();
        let mut pos = start;
        while pos < end {
            match bytes[pos] {
                b'\\' => pos = self.escape(pos, &mut text),
                b'{' if bytes.get(pos + 1) == Some(&b'{') => {
                    text.push('{');
                    pos += 2;
                }
                b'{' => {
                    let close = scan_interpolation(bytes, pos)
                        .unwrap_or(end + 1)
                        .min(end + 1);
                    if !text.is_empty() {
                        let symbol = self.ast.symbols.intern(&std::mem::take(&mut text));
                        self.ast.push_to(StrPart::Text(symbol));
                    }
                    if let Some(part) = self.interpolation(pos + 1, close - 1) {
                        self.ast.push_to(part);
                    }
                    pos = close;
                }
                _ => {
                    // Copy a run of ordinary text in one go.
                    let run = bytes[pos..end]
                        .iter()
                        .position(|&b| matches!(b, b'\\' | b'{'))
                        .map_or(end, |n| pos + n);
                    text.push_str(&self.source[pos..run]);
                    pos = run;
                }
            }
        }
        if !text.is_empty() {
            let symbol = self.ast.symbols.intern(&text);
            self.ast.push_to(StrPart::Text(symbol));
        }
        self.ast.finish(mark)
    }

    /// Decodes the escape at `pos` into `text`, returning the position after it.
    fn escape(&mut self, pos: usize, text: &mut String) -> usize {
        let bytes = self.source.as_bytes();
        let decoded = match bytes.get(pos + 1) {
            Some(b'n') => '\n',
            Some(b't') => '\t',
            Some(b'r') => '\r',
            Some(b'0') => '\0',
            Some(b'\\') => '\\',
            Some(b'"') => '"',
            Some(b'u') if bytes.get(pos + 2) == Some(&b'{') => {
                let digits_start = pos + 3;
                let close = bytes[digits_start..]
                    .iter()
                    .position(|&b| b == b'}')
                    .map(|n| digits_start + n);
                let value = close
                    .and_then(|c| u32::from_str_radix(&self.source[digits_start..c], 16).ok())
                    .and_then(char::from_u32);
                match (close, value) {
                    (Some(c), Some(ch)) => {
                        text.push(ch);
                        return c + 1;
                    }
                    _ => {
                        let end = close.map_or(pos + 2, |c| c + 1);
                        let at = Span::new(self.offset_of(pos), self.offset_of(end));
                        self.error(at, "invalid Unicode escape").with_help_mut(
                            "write it as `\\u{1F600}`, with 1 to 6 hexadecimal digits",
                        );
                        return end;
                    }
                }
            }
            _ => {
                let len = self.source[pos + 1..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8);
                let at = Span::new(self.offset_of(pos), self.offset_of(pos + 1 + len));
                self.error(at, "unknown escape")
                    .with_help_mut("the escapes are \\n, \\t, \\r, \\0, \\\\, \\\" and \\u{…}");
                return pos + 1 + len;
            }
        };
        text.push(decoded);
        pos + 2
    }

    /// The interpolation whose contents run from `start` to `end`: an expression and an
    /// optional format spec after a top-level `:`.
    fn interpolation(&mut self, start: usize, end: usize) -> Option<StrPart> {
        let bytes = self.source.as_bytes();
        let span = Span::new(self.offset_of(start) - 1, self.offset_of(end) + 1);
        // The spec starts at the first `:` outside brackets and nested strings that is not
        // part of `::`.
        let mut depth = 0usize;
        let mut colon = None;
        let mut pos = start;
        while pos < end {
            match bytes[pos] {
                b'"' => {
                    pos = scan_string(bytes, pos).unwrap_or(end);
                    continue;
                }
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                b':' if bytes.get(pos + 1) == Some(&b':') => {
                    pos += 2;
                    continue;
                }
                b':' if depth == 0 => {
                    colon = Some(pos);
                    break;
                }
                _ => {}
            }
            pos += 1;
        }
        let expr_end = colon.unwrap_or(end);
        if self.source[start..expr_end].trim().is_empty() {
            self.error(span, "empty interpolation")
                .with_help_mut("write `{{` for a literal brace");
            return None;
        }
        let expr = self.sub_expr(self.offset_of(start), self.offset_of(expr_end));
        let spec = colon.and_then(|c| self.format_spec(c + 1, end));
        Some(StrPart::Value { expr, spec })
    }

    /// Parses the source from offset `start` to offset `end` as one expression.
    fn sub_expr(&mut self, start: u32, end: u32) -> ExprId {
        let text = &self.source[self.pos_of(start)..self.pos_of(end)];
        let mut lexed = lex_at(text, start);
        lexed.tokens.retain(|t| t.kind != TokenKind::Newline);
        self.diagnostics.extend(lexed.diagnostics);
        let saved_tokens = std::mem::replace(&mut self.tokens, lexed.tokens);
        let saved_pos = std::mem::replace(&mut self.pos, 0);
        let saved_no_struct = std::mem::replace(&mut self.no_struct, false);
        let expr = self.expr();
        if !self.at(TokenKind::Eof) {
            self.expected("the end of the interpolated expression");
        }
        self.tokens = saved_tokens;
        self.pos = saved_pos;
        self.no_struct = saved_no_struct;
        expr
    }

    /// `[[fill] align] ["+"] ["0"] [width] ["." precision] [type]`.
    fn format_spec(&mut self, start: usize, end: usize) -> Option<FormatSpec> {
        let text = &self.source[start..end];
        let span = Span::new(self.offset_of(start), self.offset_of(end));
        let mut chars = text.chars().peekable();
        let align_of = |c: char| match c {
            '<' => Some(Align::Left),
            '>' => Some(Align::Right),
            '^' => Some(Align::Center),
            _ => None,
        };
        let mut spec = FormatSpec {
            fill: ' ',
            align: None,
            plus: false,
            zero: false,
            width: None,
            precision: None,
            kind: None,
            span,
        };
        // A fill character is only present when an alignment follows it.
        let first_two: Vec<char> = text.chars().take(2).collect();
        if first_two.len() == 2 && align_of(first_two[1]).is_some() {
            spec.fill = first_two[0];
            spec.align = align_of(first_two[1]);
            chars.next();
            chars.next();
        } else if let Some(a) = first_two.first().and_then(|&c| align_of(c)) {
            spec.align = Some(a);
            chars.next();
        }
        if chars.peek() == Some(&'+') {
            spec.plus = true;
            chars.next();
        }
        if chars.peek() == Some(&'0') {
            spec.zero = true;
            chars.next();
        }
        let number = |chars: &mut std::iter::Peekable<std::str::Chars>| {
            let mut digits = String::new();
            while let Some(&c) = chars.peek().filter(|c| c.is_ascii_digit()) {
                digits.push(c);
                chars.next();
            }
            (!digits.is_empty())
                .then(|| digits.parse::<u32>().ok())
                .flatten()
        };
        spec.width = number(&mut chars);
        if chars.peek() == Some(&'.') {
            chars.next();
            spec.precision = number(&mut chars);
            if spec.precision.is_none() {
                self.error(span, "expected a precision after `.`");
                return None;
            }
        }
        spec.kind = match chars.next() {
            None => None,
            Some('b') => Some(FormatKind::Binary),
            Some('o') => Some(FormatKind::Octal),
            Some('x') => Some(FormatKind::Hex),
            Some('X') => Some(FormatKind::HexUpper),
            Some('e') => Some(FormatKind::Exponent),
            Some(_) => {
                self.error(span, format!("invalid format spec `{text}`"))
                    .with_help_mut(
                        "see spec/types.md §7: for example `:>8`, `:.3`, `:08b`, `:x`, `:e`",
                    );
                return None;
            }
        };
        if chars.next().is_some() {
            self.error(span, format!("invalid format spec `{text}`"))
                .with_help_mut(
                    "see spec/types.md §7: for example `:>8`, `:.3`, `:08b`, `:x`, `:e`",
                );
            return None;
        }
        Some(spec)
    }
}

/// In-place builders, for adding to a diagnostic after it has been pushed.
trait DiagnosticExt {
    fn with_help_mut(&mut self, help: impl Into<String>) -> &mut Self;
}

impl DiagnosticExt for Diagnostic {
    fn with_help_mut(&mut self, help: impl Into<String>) -> &mut Self {
        self.help = Some(help.into());
        self
    }
}

/// An integer literal's value (decimal, `0x…` or `0b…`, with `_` between digits), if it fits
/// in a `u64`.
fn parse_u64(text: &str) -> Option<u64> {
    let text = text.replace('_', "");
    if let Some(hex) = text.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).ok()
    } else if let Some(bin) = text.strip_prefix("0b") {
        u64::from_str_radix(bin, 2).ok()
    } else {
        text.parse().ok()
    }
}

#[cfg(test)]
mod tests;
