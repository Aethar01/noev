//! The syntax tree.
//!
//! Nodes live in flat arrays inside [`Ast`] and refer to each other by `u32` ids. Children of
//! variable length are contiguous runs ([`List`]) in shared pools, so building a tree
//! allocates only when an array grows, never per node.

use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroU32;

use crate::span::Span;
use crate::symbol::{Interner, Symbol};

macro_rules! ids {
    ($($(#[$doc:meta])* $name:ident;)*) => {$(
        $(#[$doc])*
        ///
        /// Stored as index + 1 so that `Option` of an id is still 4 bytes.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(NonZeroU32);

        impl $name {
            pub(crate) fn new(index: usize) -> Self {
                let raw = u32::try_from(index + 1).expect("fewer than 2³² nodes");
                Self(NonZeroU32::new(raw).expect("index + 1 is not zero"))
            }

            /// The node's position in its array.
            pub fn index(self) -> usize {
                self.0.get() as usize - 1
            }

            /// The id of the node at `index` in its array.
            pub fn from_index(index: usize) -> Self {
                Self::new(index)
            }
        }
    )*};
}

ids! {
    /// An expression.
    ExprId;
    /// A statement.
    StmtId;
    /// A block.
    BlockId;
    /// A type annotation.
    TypeId;
    /// A pattern.
    PatId;
    /// A top-level item.
    ItemId;
    /// A function or method.
    FnId;
}

/// A contiguous run of `T` in one of the [`Ast`]'s pools.
pub struct List<T> {
    start: u32,
    len: u32,
    _marker: PhantomData<T>,
}

impl<T> List<T> {
    /// The empty list.
    pub const EMPTY: Self = List {
        start: 0,
        len: 0,
        _marker: PhantomData,
    };

    /// How many elements there are.
    pub fn len(self) -> usize {
        self.len as usize
    }

    /// Whether there are no elements.
    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

impl<T> Clone for List<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for List<T> {}

impl<T> PartialEq for List<T> {
    fn eq(&self, other: &Self) -> bool {
        (self.start, self.len) == (other.start, other.len)
    }
}

impl<T> fmt::Debug for List<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "List({}..{})", self.start, self.start + self.len)
    }
}

/// A pool of list elements, plus a scratch stack used while a list is being built.
///
/// Lists nest (a call argument can contain another call), so elements are pushed onto the
/// scratch stack and moved into the pool in one piece when their list is finished. Nested
/// lists finish first, so each list's elements are contiguous on the stack.
#[derive(Debug)]
pub struct Pool<T> {
    items: Vec<T>,
    scratch: Vec<T>,
}

impl<T> Default for Pool<T> {
    fn default() -> Self {
        Pool {
            items: Vec::new(),
            scratch: Vec::new(),
        }
    }
}

impl<T: Copy> Pool<T> {
    pub(crate) fn mark(&self) -> usize {
        self.scratch.len()
    }

    pub(crate) fn push(&mut self, item: T) {
        self.scratch.push(item);
    }

    pub(crate) fn finish(&mut self, mark: usize) -> List<T> {
        let start = self.items.len();
        self.items.extend(self.scratch.drain(mark..));
        List {
            start: u32::try_from(start).expect("fewer than 2³² list elements"),
            len: u32::try_from(self.items.len() - start).expect("fewer than 2³² list elements"),
            _marker: PhantomData,
        }
    }

    fn get(&self, list: List<T>) -> &[T] {
        &self.items[list.start as usize..(list.start + list.len) as usize]
    }
}

/// Element types stored in an [`Ast`] pool.
pub trait Pooled: Copy + Sized {
    #[doc(hidden)]
    fn pool(ast: &Ast) -> &Pool<Self>;
    #[doc(hidden)]
    fn pool_mut(ast: &mut Ast) -> &mut Pool<Self>;
}

macro_rules! pools {
    ($($field:ident: $ty:ty,)*) => {
        /// Pools for every list element type.
        #[derive(Debug, Default)]
        pub struct Pools {
            $($field: Pool<$ty>,)*
        }

        $(impl Pooled for $ty {
            fn pool(ast: &Ast) -> &Pool<Self> {
                &ast.pools.$field
            }
            fn pool_mut(ast: &mut Ast) -> &mut Pool<Self> {
                &mut ast.pools.$field
            }
        })*
    };
}

pools! {
    exprs: ExprId,
    stmts: StmtId,
    types: TypeId,
    pats: PatId,
    items: ItemId,
    fns: FnId,
    symbols: Symbol,
    params: Param,
    fields: FieldDecl,
    field_inits: FieldInit,
    type_aliases: TypeAlias,
    str_parts: StrPart,
    variants: VariantDecl,
    arms: Arm,
    field_pats: FieldPat,
    ket_parts: KetPart,
}

/// The variants of `Option` and `Result`, named without their enum: alone, each is a path
/// (`ExprKind::Path`), never a variable (`spec/grammar.md` §2).
pub const BARE_VARIANTS: [&str; 4] = ["Some", "None", "Ok", "Err"];

/// Names the standard library declares, interned in every file: the enum `Basis` and its
/// variants and `State` and its (`spec/std.md` §2), and `Option` and `Result` and theirs
/// (`spec/types.md` §2).
pub const STD_NAMES: [&str; 17] = [
    "Basis", "X", "Y", "Z", "Option", "Result", "Some", "None", "Ok", "Err", "State", "Zero",
    "One", "Plus", "Minus", "PlusI", "MinusI",
];

/// A parsed file.
#[derive(Debug, Default)]
pub struct Ast {
    /// The file's top-level items, in order.
    pub root: List<ItemId>,
    /// Identifier names.
    pub symbols: Interner,
    exprs: Vec<Expr>,
    stmts: Vec<Stmt>,
    blocks: Vec<Block>,
    types: Vec<TypeExpr>,
    pats: Vec<Pat>,
    items: Vec<Item>,
    fns: Vec<FnDecl>,
    pools: Pools,
}

impl<T> Default for List<T> {
    fn default() -> Self {
        List::EMPTY
    }
}

macro_rules! arena {
    ($($push:ident, $get:ident, $all:ident: $id:ident => $node:ident in $field:ident;)*) => {
        impl Ast {
            $(
                pub(crate) fn $push(&mut self, node: $node) -> $id {
                    self.$field.push(node);
                    $id::new(self.$field.len() - 1)
                }

                /// The node with this id.
                pub fn $get(&self, id: $id) -> &$node {
                    &self.$field[id.index()]
                }

                /// Every node of this kind, in the order they were created.
                pub fn $all(&self) -> &[$node] {
                    &self.$field
                }
            )*
        }
    };
}

arena! {
    push_expr, expr, all_exprs: ExprId => Expr in exprs;
    push_stmt, stmt, all_stmts: StmtId => Stmt in stmts;
    push_block, block, all_blocks: BlockId => Block in blocks;
    push_type, ty, all_types: TypeId => TypeExpr in types;
    push_pat, pat, all_pats: PatId => Pat in pats;
    push_item, item, all_items: ItemId => Item in items;
    push_fn, function, all_fns: FnId => FnDecl in fns;
}

impl Ast {
    /// The elements of `list`.
    pub fn list<T: Pooled>(&self, list: List<T>) -> &[T] {
        T::pool(self).get(list)
    }

    pub(crate) fn mark<T: Pooled>(&self) -> usize {
        T::pool(self).mark()
    }

    pub(crate) fn push_to<T: Pooled>(&mut self, item: T) {
        T::pool_mut(self).push(item);
    }

    pub(crate) fn finish<T: Pooled>(&mut self, mark: usize) -> List<T> {
        T::pool_mut(self).finish(mark)
    }

    /// The text of `symbol`.
    pub fn name(&self, symbol: Symbol) -> &str {
        self.symbols.name(symbol)
    }
}

/// An expression and where it is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Expr {
    /// What kind of expression.
    pub kind: ExprKind,
    /// Where it is.
    pub span: Span,
}

/// The kinds of expression.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ExprKind {
    /// Something that could not be parsed. Already reported.
    Error,
    /// An integer literal.
    Int(u64),
    /// An integer literal too large for a `u64`; its value is the text of the span.
    BigInt,
    /// A float literal.
    Float(f64),
    /// An imaginary literal, `2.0i`: its imaginary part.
    Imag(f64),
    /// `true` or `false`.
    Bool(bool),
    /// A ket literal such as `|+0>`; the state is the text of the span.
    Ket {
        /// How many fixed labels it has: one qubit each.
        qubits: u32,
        /// Its labels in order, when some are computed (`|0{b}>`); empty when all are fixed.
        parts: List<KetPart>,
    },
    /// A string literal, as text and interpolated values in order.
    Str(List<StrPart>),
    /// A single name: a variable, function, type, `self` or `Self`.
    Name(Symbol),
    /// A path with `::`, such as `Experiment::new`.
    Path(List<Symbol>),
    /// `()`.
    Unit,
    /// `(a, b)`.
    Tuple(List<ExprId>),
    /// `[a, b]`.
    Array(List<ExprId>),
    /// `[value; count]`.
    ArrayRepeat {
        /// The repeated value.
        value: ExprId,
        /// How many times.
        count: ExprId,
    },
    /// `Name { field: value, … }`.
    StructLit {
        /// The struct's name or path.
        path: ExprId,
        /// The fields, in source order.
        fields: List<FieldInit>,
    },
    /// `base.name`.
    Field {
        /// The value whose field is read.
        base: ExprId,
        /// The field.
        name: Symbol,
    },
    /// `base.0`.
    TupleIndex {
        /// The tuple.
        base: ExprId,
        /// The element.
        index: u32,
    },
    /// `base[index]`.
    Index {
        /// The array.
        base: ExprId,
        /// The index.
        index: ExprId,
    },
    /// `callee(args)`.
    Call {
        /// The function, usually a name or path.
        callee: ExprId,
        /// The arguments.
        args: List<ExprId>,
    },
    /// `receiver.method(args)`.
    MethodCall {
        /// The value the method is called on.
        receiver: ExprId,
        /// The method.
        method: Symbol,
        /// The arguments after the receiver.
        args: List<ExprId>,
    },
    /// `-x`, `~x`, `not x`.
    Unary {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        operand: ExprId,
    },
    /// `lhs op rhs`.
    Binary {
        /// The operator.
        op: BinaryOp,
        /// The left operand.
        lhs: ExprId,
        /// The right operand.
        rhs: ExprId,
    },
    /// `mv place`, `ref place`, `mut place`.
    Own {
        /// Which ownership word.
        kind: OwnKind,
        /// The place it applies to.
        place: ExprId,
    },
    /// `adj call`.
    Adj(ExprId),
    /// `<s| operand`: a measurement test.
    BraTest {
        /// The bra literal, including `<` and `|`.
        state: Span,
        /// How many fixed labels it has: one qubit each.
        qubits: u32,
        /// Its labels in order, when some are computed; empty when all are fixed.
        parts: List<KetPart>,
        /// The value measured.
        operand: ExprId,
    },
    /// `<ψ| x`: a measurement test against the state a preparation makes, by undoing it on
    /// `x` and testing for all zeros (`spec/ownership.md` §12).
    PrepTest {
        /// The preparation: a call.
        prep: ExprId,
        /// The value measured.
        operand: ExprId,
    },
    /// `<ψ| observable |ψ>`: an expectation value. Both sides were checked to be the same
    /// preparation, so only one is kept.
    Expectation {
        /// The preparation: a call or a ket.
        prep: ExprId,
        /// The observable.
        observable: ExprId,
        /// `within ε`: the standard error wanted when it is estimated.
        precision: Option<ExprId>,
    },
    /// `{ … }`.
    Block(BlockId),
    /// `if cond { … } else …`.
    If {
        /// The condition.
        cond: ExprId,
        /// The `then` block.
        then: BlockId,
        /// The `else` block or `else if`, if any.
        otherwise: Option<ExprId>,
    },
    /// `while cond { … }`.
    While {
        /// The condition.
        cond: ExprId,
        /// The body.
        body: BlockId,
    },
    /// `sample shots { … }`: the distribution of the block's results over `shots` separate
    /// runs of it.
    Sample {
        /// How many runs.
        shots: ExprId,
        /// The run.
        body: BlockId,
    },
    /// `match scrutinee { pattern => body, … }`.
    Match {
        /// The value matched.
        scrutinee: ExprId,
        /// The arms, in order.
        arms: List<Arm>,
    },
    /// `for var in start..end { … }`.
    For {
        /// The loop variable.
        var: Symbol,
        /// The first value.
        start: ExprId,
        /// One past the last value.
        end: ExprId,
        /// The body.
        body: BlockId,
    },
    /// `ctrl a, b { … }`.
    Ctrl {
        /// The control places.
        controls: List<ExprId>,
        /// The controlled body.
        body: BlockId,
    },
    /// `with { … } do { … }`.
    With {
        /// The block that is undone afterwards.
        with: BlockId,
        /// The block run in between.
        body: BlockId,
    },
    /// `unsafe { … }`.
    Unsafe(BlockId),
}

/// Unary operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    /// `-`
    Neg,
    /// `~`
    BitNot,
    /// `not`
    Not,
}

/// Binary operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(missing_docs)] // Each variant is named for its operator.
pub enum BinaryOp {
    Or,
    And,
    Eq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    BitOr,
    BitXor,
    BitAnd,
    Shl,
    Shr,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

impl BinaryOp {
    /// How the operator is written.
    pub fn as_str(self) -> &'static str {
        use BinaryOp::*;
        match self {
            Or => "or",
            And => "and",
            Eq => "==",
            NotEq => "!=",
            Lt => "<",
            Le => "<=",
            Gt => ">",
            Ge => ">=",
            BitOr => "|",
            BitXor => "^",
            BitAnd => "&",
            Shl => "<<",
            Shr => ">>",
            Add => "+",
            Sub => "-",
            Mul => "*",
            Div => "/",
            Rem => "%",
        }
    }
}

/// Assignment operators: `=` and the compound forms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    /// `=`
    Assign,
    /// `op=`, such as `+=`.
    Compound(BinaryOp),
}

/// The three ownership words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnKind {
    /// `mv`: give the value away.
    Mv,
    /// `ref`: lend it to be read.
    Ref,
    /// `mut`: lend it to be changed.
    Mut,
}

impl OwnKind {
    /// How the word is written.
    pub fn as_str(self) -> &'static str {
        match self {
            OwnKind::Mv => "mv",
            OwnKind::Ref => "ref",
            OwnKind::Mut => "mut",
        }
    }
}

/// A piece of a ket or bra with computed labels (`spec/types.md` §3).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KetPart {
    /// A fixed label, such as `0` or `(+i)`.
    Label(crate::state::Label),
    /// `{expr}`: a `Bit`, `[Bit; N]`, `State` or `[State; N]`.
    Value(ExprId),
}

/// A piece of a string literal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StrPart {
    /// Literal text, with escapes already decoded.
    Text(Symbol),
    /// `{expr}` or `{expr:spec}`.
    Value {
        /// The interpolated expression.
        expr: ExprId,
        /// How to format it.
        spec: Option<FormatSpec>,
    },
}

/// A format spec: `{x:>8.3}` and the like (`spec/types.md` §7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatSpec {
    /// The padding character; a space unless given.
    pub fill: char,
    /// Alignment within `width`; the type's default when absent.
    pub align: Option<Align>,
    /// `+`: always show the sign.
    pub plus: bool,
    /// `0`: pad numbers with zeros after the sign.
    pub zero: bool,
    /// The minimum width.
    pub width: Option<u32>,
    /// Digits after the decimal point.
    pub precision: Option<u32>,
    /// Another representation: binary, octal, hexadecimal or exponent.
    pub kind: Option<FormatKind>,
    /// Where the spec is written.
    pub span: Span,
}

/// Alignment in a format spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    /// `<`
    Left,
    /// `>`
    Right,
    /// `^`
    Center,
}

/// A format spec's representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatKind {
    /// `b`
    Binary,
    /// `o`
    Octal,
    /// `x`
    Hex,
    /// `X`
    HexUpper,
    /// `e`
    Exponent,
}

/// `name: value` in a struct literal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FieldInit {
    /// The field.
    pub name: Symbol,
    /// The value, or `None` for the shorthand `name`, meaning `name: name`.
    pub value: Option<ExprId>,
    /// Where the field initialiser is.
    pub span: Span,
}

/// A block of statements.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Block {
    /// The statements, in order.
    pub stmts: List<StmtId>,
    /// From `{` to `}`.
    pub span: Span,
}

/// A statement and where it is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stmt {
    /// What kind of statement.
    pub kind: StmtKind,
    /// Where it is.
    pub span: Span,
}

/// The kinds of statement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StmtKind {
    /// `let [mut] pat [: ty] = init`.
    Let {
        /// Whether it is `let mut`.
        mutable: bool,
        /// What is bound.
        pat: PatId,
        /// The annotated type, if any.
        ty: Option<TypeId>,
        /// The value.
        init: ExprId,
    },
    /// `place op value`.
    Assign {
        /// `=` or a compound operator.
        op: AssignOp,
        /// What is assigned to.
        place: ExprId,
        /// The new value.
        value: ExprId,
    },
    /// `return [value]`.
    Return(Option<ExprId>),
    /// `break`.
    Break,
    /// `continue`.
    Continue,
    /// `assert cond`.
    Assert(ExprId),
    /// An expression used as a statement.
    Expr {
        /// The expression.
        expr: ExprId,
        /// Whether a `;` follows it. A block's value is its last statement if that is an
        /// expression without one.
        semi: bool,
    },
}

/// A pattern and where it is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pat {
    /// What kind of pattern.
    pub kind: PatKind,
    /// Where it is.
    pub span: Span,
}

/// The kinds of pattern.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PatKind {
    /// A variable.
    Name(Symbol),
    /// `_`.
    Wildcard,
    /// `(a, b)`.
    Tuple(List<PatId>),
    /// `[a, b]`: one pattern per element of an array.
    Array(List<PatId>),
    /// An enum variant: `Basis::X`, `Some(q)`, `Outcome::Retry { attempts }`.
    Variant {
        /// The variant's path: `[Basis, X]`, or one name for `Some`, `None`, `Ok`, `Err`.
        path: List<Symbol>,
        /// The variant's data, if the pattern takes it apart.
        data: PatData,
    },
    /// A literal the value must equal: `0`, `-1`, `true`, `"yes"`.
    Literal(PatLiteral),
}

/// The data part of a variant pattern.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PatData {
    /// No data: `Basis::X`.
    None,
    /// Values: `Some(q)`.
    Tuple(List<PatId>),
    /// Named fields: `Outcome::Retry { attempts }`, with `..` for the rest.
    Fields {
        /// The fields named; a field alone binds a variable of its name.
        fields: List<FieldPat>,
        /// Whether `..` ends the list.
        rest: bool,
    },
}

/// `field: pattern`, or `field` alone, in a variant pattern.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FieldPat {
    /// The field.
    pub name: Symbol,
    /// Its pattern. The parser makes `field` alone into `field: field`, so it is always
    /// there.
    pub pat: Option<PatId>,
    /// Where it is.
    pub span: Span,
}

/// A literal in a pattern.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PatLiteral {
    /// An integer (also a `Bit` where one is expected).
    Int(i64),
    /// `true` or `false`.
    Bool(bool),
    /// A string, interned.
    Str(Symbol),
}

/// One arm of a `match`: `pattern => body`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Arm {
    /// What the value must fit.
    pub pat: PatId,
    /// The arm's value.
    pub body: ExprId,
    /// The whole arm.
    pub span: Span,
}

/// An enum variant declaration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VariantDecl {
    /// The variant's name.
    pub name: Symbol,
    /// Its data.
    pub data: VariantData,
    /// Where it is.
    pub span: Span,
}

/// What an enum variant holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VariantData {
    /// Nothing.
    Unit,
    /// Values, by position: `Success(Bit)`.
    Tuple(List<TypeId>),
    /// Named fields: `Retry { attempts: Int }`.
    Fields(List<FieldDecl>),
}

/// A type annotation and where it is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TypeExpr {
    /// What kind of type.
    pub kind: TypeKind,
    /// Where it is.
    pub span: Span,
}

/// The kinds of type annotation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TypeKind {
    /// Something that could not be parsed. Already reported.
    Error,
    /// A named type, such as `Int` or `Experiment`, or a path.
    Path(List<Symbol>),
    /// A named type with type arguments, such as `Distribution<(Bit, Bit)>`.
    Generic {
        /// The type's name or path.
        path: List<Symbol>,
        /// The type arguments.
        args: List<TypeId>,
    },
    /// `ref T` or `mut T`.
    Ref {
        /// Whether it is `mut T`.
        mutable: bool,
        /// The referenced type.
        inner: TypeId,
    },
    /// `()`.
    Unit,
    /// `(A, B)`.
    Tuple(List<TypeId>),
    /// `[T; N]`.
    Array {
        /// The element type.
        elem: TypeId,
        /// The length.
        len: u64,
    },
}

/// A top-level item and where it is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Item {
    /// What kind of item.
    pub kind: ItemKind,
    /// Where it is.
    pub span: Span,
}

/// The kinds of top-level item.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ItemKind {
    /// Something that could not be parsed, or is not supported yet. Already reported.
    Error,
    /// A function.
    Fn(FnId),
    /// A struct with its fields and methods.
    Struct {
        /// The struct's name.
        name: Symbol,
        /// Where the name is.
        name_span: Span,
        /// The fields, in declaration order.
        fields: List<FieldDecl>,
        /// Methods and associated functions.
        methods: List<FnId>,
    },
    /// An enum with its variants and methods.
    Enum {
        /// The enum's name.
        name: Symbol,
        /// Where the name is.
        name_span: Span,
        /// The variants, in declaration order.
        variants: List<VariantDecl>,
        /// Methods and associated functions.
        methods: List<FnId>,
    },
    /// `type Name = Type`: another name for a type.
    Type {
        /// The alias.
        name: Symbol,
        /// Where the name is.
        name_span: Span,
        /// The type it names.
        ty: TypeId,
    },
    /// `trait Name { … }` (Reserved).
    Trait {
        /// The trait's name.
        name: Symbol,
        /// Required and provided functions.
        fns: List<FnId>,
        /// Associated type names.
        types: List<Symbol>,
    },
    /// `impl Trait for Type { … }` (Reserved).
    Impl {
        /// The trait's path.
        trait_path: List<Symbol>,
        /// The type it is implemented for.
        ty: TypeId,
        /// The functions.
        fns: List<FnId>,
        /// Associated type definitions.
        types: List<TypeAlias>,
    },
}

/// A function or method declaration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FnDecl {
    /// The function's name.
    pub name: Symbol,
    /// Where the name is.
    pub name_span: Span,
    /// Whether it is `unsafe fn`.
    pub is_unsafe: bool,
    /// Whether it is `rev fn`.
    pub is_rev: bool,
    /// Whether it is `circuit fn`.
    pub is_circuit: bool,
    /// `mv self`, `ref self` or `mut self`, for methods.
    pub receiver: Option<Receiver>,
    /// The other parameters.
    pub params: List<Param>,
    /// The return type, if not `()`.
    pub ret: Option<TypeId>,
    /// The body. Only trait declarations may omit it.
    pub body: Option<BlockId>,
    /// The whole declaration.
    pub span: Span,
}

/// A method's receiver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Receiver {
    /// How the method takes `self`.
    pub kind: OwnKind,
    /// Where it is.
    pub span: Span,
}

/// A function parameter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Param {
    /// The parameter's name.
    pub name: Symbol,
    /// Its type.
    pub ty: TypeId,
    /// Where it is.
    pub span: Span,
}

/// A struct field declaration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FieldDecl {
    /// The field's name.
    pub name: Symbol,
    /// Its type.
    pub ty: TypeId,
    /// Where it is.
    pub span: Span,
}

/// `type Name = Type` in an `impl` block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TypeAlias {
    /// The associated type's name.
    pub name: Symbol,
    /// What it is.
    pub ty: TypeId,
}
