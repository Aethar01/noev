//! Types, interned so that comparing two types is comparing two integers.

use noev_syntax::ast::BinaryOp;
use noev_syntax::{Ast, FxHashMap, Span, Symbol};

/// What a `std::ops` trait stands for (`spec/types.md` §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    /// A binary operator: `Add` for `a + b`.
    Binary(BinaryOp),
    /// Unary `-`.
    Neg,
    /// A compound assignment: `AddAssign` for `a += b`.
    Assign(BinaryOp),
}

/// A `std::ops` trait: its name, how its operator is written, and what it stands for.
#[derive(Clone, Copy, Debug)]
pub struct OpTrait {
    /// The trait's name.
    pub name: &'static str,
    /// How the operator is written.
    pub word: &'static str,
    /// What it stands for.
    pub kind: OpKind,
}

const fn op(name: &'static str, word: &'static str, kind: OpKind) -> OpTrait {
    OpTrait { name, word, kind }
}

/// `std::ops`'s traits, in the order of [`Types::ops`].
pub const OPS: [OpTrait; 11] = [
    op("Add", "+", OpKind::Binary(BinaryOp::Add)),
    op("Sub", "-", OpKind::Binary(BinaryOp::Sub)),
    op("Mul", "*", OpKind::Binary(BinaryOp::Mul)),
    op("Div", "/", OpKind::Binary(BinaryOp::Div)),
    op("Rem", "%", OpKind::Binary(BinaryOp::Rem)),
    op("Neg", "-", OpKind::Neg),
    op("AddAssign", "+=", OpKind::Assign(BinaryOp::Add)),
    op("SubAssign", "-=", OpKind::Assign(BinaryOp::Sub)),
    op("MulAssign", "*=", OpKind::Assign(BinaryOp::Mul)),
    op("DivAssign", "/=", OpKind::Assign(BinaryOp::Div)),
    op("RemAssign", "%=", OpKind::Assign(BinaryOp::Rem)),
];

/// [`OPS`]'s `Neg`.
pub const NEG: usize = 5;

/// The `std::ops` trait for a binary operator, or for its compound assignment, by position
/// in [`OPS`].
pub fn op_index(op: BinaryOp, assign: bool) -> Option<usize> {
    OPS.iter().position(|o| {
        o.kind
            == if assign {
                OpKind::Assign(op)
            } else {
                OpKind::Binary(op)
            }
    })
}

/// An interned type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ty(u32);

/// A struct declared in the program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StructId(pub(crate) u32);

impl StructId {
    /// The struct's position in [`Types::structs`].
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// The struct at `index` in [`Types::structs`].
    pub fn from_index(index: usize) -> Self {
        Self(u32::try_from(index).expect("fewer than 2³² types"))
    }
}

/// What a type is.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TyKind {
    /// The type of something that failed to check. Already reported; compatible with
    /// everything, so one mistake does not cause more.
    Error,
    /// The type of code that never finishes normally, such as a block ending in `return`.
    /// Compatible with every type.
    Never,
    /// `()`.
    Unit,
    /// `Int`.
    Int,
    /// `Float`.
    Float,
    /// `Bool`.
    Bool,
    /// `Bit`.
    Bit,
    /// `Qubit`.
    Qubit,
    /// `Observable`.
    Observable,
    /// `String`.
    String,
    /// `i8`.
    I8,
    /// `i16`.
    I16,
    /// `i32`.
    I32,
    /// `i64`.
    I64,
    /// `u8`.
    U8,
    /// `u16`.
    U16,
    /// `u32`.
    U32,
    /// `u64`.
    U64,
    /// `f32`. (`f64` is another name for `Float`.)
    F32,
    /// `Complex`, two `Float`s (also called `c64`).
    Complex,
    /// `c32`, two `f32`s.
    C32,
    /// `(A, B, …)`.
    Tuple(Box<[Ty]>),
    /// `[T; N]`.
    Array(Ty, u64),
    /// `[T; N]` for a size parameter `N`, inside generic code: an array whose length is
    /// known in each copy (`spec/types.md` §6).
    SizedArray(Ty, Ty),
    /// A size given as a type argument: the `4` of `Reg<4>`.
    Size(u64),
    /// A size computed from size parameters, `constant + k·N + …`, such as the `N + 1` of
    /// `[Qubit; N + 1]` (`spec/types.md` §6). Made only by [`Types::size_sum`], so it has at
    /// least one term and is not a parameter alone, its terms are sorted and their factors
    /// positive, and equal sums are the same type.
    SizeSum {
        /// The constant.
        constant: u64,
        /// Each size parameter, with its factor.
        terms: Box<[(Ty, u64)]>,
    },
    /// `Distribution<T>`.
    Distribution(Ty),
    /// A struct.
    Struct(StructId),
    /// `ref T` or `mut T`.
    Ref {
        /// Whether it is `mut T`.
        mutable: bool,
        /// The referenced type.
        inner: Ty,
    },
    /// A type parameter, inside generic code: any type its bounds allow (`spec/types.md`
    /// §6).
    Param(ParamId),
    /// A trait's associated type of a type that is not known yet, such as `T::Result` for
    /// `T: Measure`. Once `base` is known, its `impl` says what it is.
    Assoc {
        /// The type whose associated type it is.
        base: Ty,
        /// The trait, by its position in the checker's table of traits.
        trait_index: u32,
        /// The trait's arguments: the `Int` of `<T as Add<Int>>::Output`.
        args: Box<[Ty]>,
        /// The associated type's name.
        name: Symbol,
        /// Whether the trait requires it to be `Copy`.
        copy: bool,
    },
}

/// A type parameter of a generic function, struct or enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ParamId(u32);

impl ParamId {
    /// The parameter's position in [`Types::params`].
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A type parameter's name and what generic code may do with its values.
#[derive(Clone, Copy, Debug)]
pub struct ParamDef {
    /// Its name.
    pub name: Symbol,
    /// Whether it is `T: Copy`: its values can be copied and dropped. Otherwise they
    /// are treated like qubits.
    pub copy: bool,
    /// Whether it is a size parameter, `const N: Int`, rather than a type.
    pub size: bool,
}

/// Whether values of a type can be copied and dropped (`spec/types.md` §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// Copied on assignment; dropped freely.
    Copyable,
    /// Never copied, but can be dropped (`mut T`).
    Affine,
    /// Never copied or dropped: must be consumed (anything containing a qubit).
    Linear,
}

/// A named type: a struct's fields, or an enum's variants. Both share one table, so methods,
/// `Self` and type names work the same for either.
#[derive(Clone, Debug)]
pub struct StructDef {
    /// The type's name.
    pub name: Symbol,
    /// Where the name is declared.
    pub span: Span,
    /// A struct's fields, in declaration order; empty for an enum.
    pub fields: Vec<(Symbol, Ty)>,
    /// An enum's variants, in declaration order; `None` for a struct.
    pub variants: Option<Vec<VariantDef>>,
    /// The type arguments of `Option<T>` or `Result<T, E>`, one such type per use; empty
    /// for other types.
    pub args: Vec<Ty>,
    /// Whether its variants are named alone (`Some(1)`, not `Option::Some(1)`).
    pub bare: bool,
    /// The generic definition this is an instance of, such as `Pair<T, U>` for
    /// `Pair<Int, Bit>`. A generic definition's own `args` are its type parameters.
    pub template: Option<StructId>,
}

/// One variant of an enum.
#[derive(Clone, Debug)]
pub struct VariantDef {
    /// The variant's name.
    pub name: Symbol,
    /// Where it is declared.
    pub span: Span,
    /// The types of its data, in order.
    pub data: Vec<Ty>,
    /// The names of its fields, for a variant with named fields; empty otherwise.
    pub names: Vec<Symbol>,
    /// Whether its data is written in braces (`Retry { attempts: Int }`).
    pub named: bool,
}

/// Every type used by a program.
#[derive(Clone, Debug)]
pub struct Types {
    kinds: Vec<TyKind>,
    map: FxHashMap<TyKind, Ty>,
    /// The program's structs and enums, after the standard library's.
    pub structs: Vec<StructDef>,
    /// The standard library's `Basis` (`spec/std.md` §2).
    pub basis: Option<Ty>,
    /// The standard library's `State` (`spec/std.md` §2): the labels as values.
    pub state: Option<Ty>,
    /// Every type parameter.
    pub params: Vec<ParamDef>,
    /// The instance of each generic definition for each list of type arguments.
    instances: FxHashMap<(StructId, Vec<Ty>), StructId>,
    /// The standard library's `Measure` trait, by its position in the checker's table of
    /// traits, once a program names it.
    pub measure: Option<u32>,
    /// `std::ops`'s traits, in the order of [`OPS`], by their positions in the checker's
    /// table of traits, once a program uses them.
    pub ops: [Option<u32>; OPS.len()],
}

macro_rules! basic {
    ($($name:ident = $index:literal: $kind:ident,)*) => {
        impl Ty {
            $(
                #[allow(missing_docs)]
                pub const $name: Ty = Ty($index);
            )*
        }

        // The built-in types sit at fixed indices and are found by matching, not hashing:
        // every check starts a table, so they must cost next to nothing.
        impl Default for Types {
            fn default() -> Self {
                Types {
                    kinds: vec![$(TyKind::$kind),*],
                    map: FxHashMap::default(),
                    structs: Vec::new(),
                    basis: None,
                    state: None,
                    params: Vec::new(),
                    instances: FxHashMap::default(),
                    measure: None,
                    ops: [None; OPS.len()],
                }
            }
        }

        /// The built-in type `kind` is, if it is one.
        fn basic(kind: &TyKind) -> Option<Ty> {
            Some(match kind {
                $(TyKind::$kind => Ty::$name,)*
                _ => return None,
            })
        }
    };
}

basic! {
    ERROR = 0: Error,
    NEVER = 1: Never,
    UNIT = 2: Unit,
    INT = 3: Int,
    FLOAT = 4: Float,
    BOOL = 5: Bool,
    BIT = 6: Bit,
    QUBIT = 7: Qubit,
    OBSERVABLE = 8: Observable,
    STRING = 9: String,
    I8 = 10: I8,
    I16 = 11: I16,
    I32 = 12: I32,
    I64 = 13: I64,
    U8 = 14: U8,
    U16 = 15: U16,
    U32 = 16: U32,
    U64 = 17: U64,
    F32 = 18: F32,
    COMPLEX = 19: Complex,
    C32 = 20: C32,
}

/// A sized integer type's size and signedness (`spec/types.md` §1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Width {
    /// How many bits it has: 8, 16, 32 or 64.
    pub bits: u32,
    /// Whether it is signed (`i8`…`i64`) rather than unsigned (`u8`…`u64`).
    pub signed: bool,
}

impl Width {
    /// The smallest value.
    pub fn min(self) -> i128 {
        if self.signed {
            -(1i128 << (self.bits - 1))
        } else {
            0
        }
    }

    /// The largest value.
    pub fn max(self) -> i128 {
        if self.signed {
            (1i128 << (self.bits - 1)) - 1
        } else {
            (1i128 << self.bits) - 1
        }
    }

    /// Whether `v` is in range.
    pub fn holds(self, v: i128) -> bool {
        (self.min()..=self.max()).contains(&v)
    }

    /// Whether every value of `self` is a value of `other`.
    pub fn fits_in(self, other: Width) -> bool {
        other.min() <= self.min() && self.max() <= other.max()
    }

    /// Its name: `u8`, `i64`.
    pub fn name(self) -> String {
        format!("{}{}", if self.signed { 'i' } else { 'u' }, self.bits)
    }
}

impl Ty {
    /// The sized integer type's width, if it is one.
    pub fn width(self) -> Option<Width> {
        let (bits, signed) = match self {
            Ty::I8 => (8, true),
            Ty::I16 => (16, true),
            Ty::I32 => (32, true),
            Ty::I64 => (64, true),
            Ty::U8 => (8, false),
            Ty::U16 => (16, false),
            Ty::U32 => (32, false),
            Ty::U64 => (64, false),
            _ => return None,
        };
        Some(Width { bits, signed })
    }

    /// The sized integer type of `width`.
    pub fn of_width(width: Width) -> Ty {
        match (width.bits, width.signed) {
            (8, true) => Ty::I8,
            (16, true) => Ty::I16,
            (32, true) => Ty::I32,
            (64, true) => Ty::I64,
            (8, false) => Ty::U8,
            (16, false) => Ty::U16,
            (32, false) => Ty::U32,
            _ => Ty::U64,
        }
    }

    /// Whether it is `Int` or a sized integer.
    pub fn is_integer(self) -> bool {
        self == Ty::INT || self.width().is_some()
    }

    /// Whether it is `Float` (also called `f64`) or `f32`.
    pub fn is_float(self) -> bool {
        matches!(self, Ty::FLOAT | Ty::F32)
    }

    /// Whether it is `Complex` (also called `c64`) or `c32`.
    pub fn is_complex(self) -> bool {
        matches!(self, Ty::COMPLEX | Ty::C32)
    }

    /// Whether it is a number: an integer or a float. (Complex numbers have no order, so
    /// they are not among them.)
    pub fn is_number(self) -> bool {
        self.is_integer() || self.is_float()
    }
}

impl Types {
    /// The type for `kind`, creating it if needed.
    pub fn intern(&mut self, kind: TyKind) -> Ty {
        if let Some(ty) = basic(&kind).or_else(|| self.map.get(&kind).copied()) {
            return ty;
        }
        // An array's length is a size, which an `impl` for `[T; N]` can match.
        if let TyKind::Array(_, len) = kind {
            self.intern(TyKind::Size(len));
        }
        let ty = Ty(u32::try_from(self.kinds.len()).expect("fewer than 2³² types"));
        self.kinds.push(kind.clone());
        self.map.insert(kind, ty);
        ty
    }

    /// The type for `kind`, if one exists.
    pub fn find(&self, kind: &TyKind) -> Option<Ty> {
        basic(kind).or_else(|| self.map.get(kind).copied())
    }

    /// What `ty` is.
    pub fn kind(&self, ty: Ty) -> &TyKind {
        &self.kinds[ty.0 as usize]
    }

    /// `[elem; len]`.
    pub fn array(&mut self, elem: Ty, len: u64) -> Ty {
        self.intern(TyKind::Array(elem, len))
    }

    /// `ref inner` or `mut inner`.
    pub fn reference(&mut self, mutable: bool, inner: Ty) -> Ty {
        self.intern(TyKind::Ref { mutable, inner })
    }

    /// A struct's type.
    pub fn struct_ty(&mut self, id: StructId) -> Ty {
        self.intern(TyKind::Struct(id))
    }

    /// A new type parameter, and its type.
    pub fn param(&mut self, name: Symbol, copy: bool) -> Ty {
        let id = ParamId(u32::try_from(self.params.len()).expect("fewer than 2³² parameters"));
        self.params.push(ParamDef {
            name,
            copy,
            size: false,
        });
        self.intern(TyKind::Param(id))
    }

    /// The name of the type or size parameter `ty`.
    pub fn param_name(&self, ty: Ty) -> Symbol {
        let id = self.as_param(ty).expect("a parameter");
        self.params[id.index()].name
    }

    /// A new size parameter, `const N: Int`, and its type.
    pub fn size_param(&mut self, name: Symbol) -> Ty {
        let id = ParamId(u32::try_from(self.params.len()).expect("fewer than 2³² parameters"));
        self.params.push(ParamDef {
            name,
            copy: true,
            size: true,
        });
        self.intern(TyKind::Param(id))
    }

    /// Whether `ty` is a size: a size parameter, a size given as an argument, or a sum of
    /// them.
    pub fn is_size(&self, ty: Ty) -> bool {
        match *self.kind(ty) {
            TyKind::Size(_) | TyKind::SizeSum { .. } => true,
            TyKind::Param(p) => self.params[p.index()].size,
            _ => false,
        }
    }

    /// A size as `constant + Σ k·N`: `None` if `ty` is not a size.
    pub fn size_parts(&self, ty: Ty) -> Option<(u64, Vec<(Ty, u64)>)> {
        match self.kind(ty) {
            &TyKind::Size(n) => Some((n, Vec::new())),
            TyKind::Param(p) if self.params[p.index()].size => Some((0, vec![(ty, 1)])),
            TyKind::SizeSum { constant, terms } => Some((*constant, terms.to_vec())),
            _ => None,
        }
    }

    /// The size `constant + Σ k·N`, normalised: a fixed size without terms, the parameter
    /// itself for `1·N`, otherwise a [`TyKind::SizeSum`]. `None` if it overflows.
    pub fn size_sum(&mut self, constant: u64, mut terms: Vec<(Ty, u64)>) -> Option<Ty> {
        terms.sort_unstable_by_key(|&(p, _)| p);
        let mut merged: Vec<(Ty, u64)> = Vec::with_capacity(terms.len());
        for (p, k) in terms {
            match merged.last_mut() {
                Some((q, j)) if *q == p => *j = j.checked_add(k)?,
                _ => merged.push((p, k)),
            }
        }
        merged.retain(|&(_, k)| k != 0);
        Some(match merged[..] {
            [] => self.intern(TyKind::Size(constant)),
            [(p, 1)] if constant == 0 => p,
            _ => self.intern(TyKind::SizeSum {
                constant,
                terms: merged.into(),
            }),
        })
    }

    /// `a + b` for sizes; `None` if either is not a size or it overflows.
    pub fn size_add(&mut self, a: Ty, b: Ty) -> Option<Ty> {
        let (c, mut terms) = self.size_parts(a)?;
        let (d, more) = self.size_parts(b)?;
        terms.extend(more);
        self.size_sum(c.checked_add(d)?, terms)
    }

    /// `k * a` for a size `a`; `None` if it is not a size or it overflows.
    pub fn size_scale(&mut self, k: u64, a: Ty) -> Option<Ty> {
        let (c, terms) = self.size_parts(a)?;
        let terms = terms
            .into_iter()
            .map(|(p, j)| Some((p, j.checked_mul(k)?)))
            .collect::<Option<Vec<_>>>()?;
        self.size_sum(c.checked_mul(k)?, terms)
    }

    /// `[elem; len]` for a size `len`: a fixed length, or a size parameter.
    pub fn sized_array(&mut self, elem: Ty, len: Ty) -> Ty {
        match *self.kind(len) {
            TyKind::Size(n) => self.array(elem, n),
            _ if self.is_wildcard(len) || self.is_wildcard(elem) => Ty::ERROR,
            _ => self.intern(TyKind::SizedArray(elem, len)),
        }
    }

    /// The element type of an array, whether or not its length is known.
    pub fn elem_of(&self, ty: Ty) -> Option<Ty> {
        match *self.kind(ty) {
            TyKind::Array(elem, _) | TyKind::SizedArray(elem, _) => Some(elem),
            _ => None,
        }
    }

    /// The type parameter `ty` is, if it is one.
    pub fn as_param(&self, ty: Ty) -> Option<ParamId> {
        match *self.kind(ty) {
            TyKind::Param(p) => Some(p),
            _ => None,
        }
    }

    /// The instance of the generic struct or enum `template` with `args` for its type
    /// parameters: its fields and variants with each parameter replaced.
    pub fn instantiate(&mut self, template: StructId, args: Vec<Ty>) -> StructId {
        if self.structs[template.index()].args == args {
            return template;
        }
        if let Some(&id) = self.instances.get(&(template, args.clone())) {
            return id;
        }
        let id = StructId::from_index(self.structs.len());
        let def = StructDef {
            fields: Vec::new(),
            variants: self.structs[template.index()]
                .variants
                .as_ref()
                .map(|_| Vec::new()),
            args: args.clone(),
            template: Some(template),
            ..self.structs[template.index()].clone()
        };
        self.structs.push(def);
        self.instances.insert((template, args), id);
        self.fill(id);
        id
    }

    /// Fills in an instance's fields and variants from its template's, as they are now.
    fn fill(&mut self, id: StructId) {
        let Some(template) = self.structs[id.index()].template else {
            return;
        };
        let from = self.structs[template.index()].args.clone();
        let to = self.structs[id.index()].args.clone();
        let fields: Vec<(Symbol, Ty)> = self.structs[template.index()].fields.clone();
        let fields = fields
            .into_iter()
            .map(|(n, t)| (n, self.subst(t, &from, &to)))
            .collect();
        let variants = self.structs[template.index()]
            .variants
            .clone()
            .map(|variants| {
                variants
                    .into_iter()
                    .map(|v| VariantDef {
                        data: v.data.iter().map(|&t| self.subst(t, &from, &to)).collect(),
                        ..v
                    })
                    .collect()
            });
        let def = &mut self.structs[id.index()];
        def.fields = fields;
        def.variants = variants;
    }

    /// Fills in every instance again, once the templates' fields and variants are known: an
    /// instance made while they were being resolved may have missed some.
    pub fn refill_instances(&mut self) {
        let mut index = 0;
        // Filling can make more instances, which are filled as they are made.
        while index < self.structs.len() {
            self.fill(StructId::from_index(index));
            index += 1;
        }
    }

    /// `ty` with each type in `from`, a type parameter, replaced by the type at the same
    /// position in `to`.
    pub fn subst(&mut self, ty: Ty, from: &[Ty], to: &[Ty]) -> Ty {
        if from.is_empty() {
            return ty;
        }
        match self.kind(ty).clone() {
            TyKind::Param(_) => from.iter().position(|&p| p == ty).map_or(ty, |i| to[i]),
            TyKind::Tuple(elems) => {
                let elems: Box<[Ty]> = elems.iter().map(|&t| self.subst(t, from, to)).collect();
                self.intern(TyKind::Tuple(elems))
            }
            TyKind::Array(elem, len) => {
                let elem = self.subst(elem, from, to);
                self.array(elem, len)
            }
            TyKind::SizedArray(elem, len) => {
                let elem = self.subst(elem, from, to);
                let len = self.subst(len, from, to);
                self.sized_array(elem, len)
            }
            TyKind::SizeSum { constant, terms } => {
                let mut sum = self.intern(TyKind::Size(constant));
                for &(p, k) in terms.iter() {
                    let p = self.subst(p, from, to);
                    let Some(next) = self.size_scale(k, p).and_then(|t| self.size_add(sum, t))
                    else {
                        return Ty::ERROR;
                    };
                    sum = next;
                }
                sum
            }
            TyKind::Distribution(elem) => {
                let elem = self.subst(elem, from, to);
                self.intern(TyKind::Distribution(elem))
            }
            TyKind::Ref { mutable, inner } => {
                let inner = self.subst(inner, from, to);
                self.reference(mutable, inner)
            }
            TyKind::Assoc {
                base,
                trait_index,
                args,
                name,
                copy,
            } => {
                let base = self.subst(base, from, to);
                let args = args.iter().map(|&a| self.subst(a, from, to)).collect();
                self.intern(TyKind::Assoc {
                    base,
                    trait_index,
                    args,
                    name,
                    copy,
                })
            }
            TyKind::Struct(id) => {
                let def = &self.structs[id.index()];
                if def.args.is_empty() {
                    return ty;
                }
                let template = def.template.unwrap_or(id);
                let old = def.args.clone();
                let args: Vec<Ty> = old.iter().map(|&t| self.subst(t, from, to)).collect();
                if args == old {
                    return ty;
                }
                let id = self.instantiate(template, args);
                self.struct_ty(id)
            }
            _ => ty,
        }
    }

    /// Whether the built-in measurement takes `ty`: a qubit or an array of qubits.
    pub fn is_measurable(&self, ty: Ty) -> bool {
        match *self.kind(ty) {
            TyKind::Qubit => true,
            TyKind::Array(elem, _) | TyKind::SizedArray(elem, _) => elem == Ty::QUBIT,
            _ => false,
        }
    }

    /// What the built-in measurement of `ty` gives: `Bit`, or `[Bit; N]`.
    pub fn measured(&mut self, ty: Ty) -> Option<Ty> {
        match *self.kind(ty) {
            TyKind::Qubit => Some(Ty::BIT),
            TyKind::Array(elem, len) if elem == Ty::QUBIT => Some(self.array(Ty::BIT, len)),
            TyKind::SizedArray(elem, len) if elem == Ty::QUBIT => {
                Some(self.sized_array(Ty::BIT, len))
            }
            _ => None,
        }
    }

    /// A type parameter `ty` mentions that may be a qubit (not `Copy`), if any: why a
    /// value of a generic type is treated like a qubit.
    pub fn quantum_param(&self, ty: Ty) -> Option<ParamId> {
        match self.kind(ty) {
            TyKind::Param(p) => (!self.params[p.index()].copy).then_some(*p),
            TyKind::Assoc {
                base, copy: false, ..
            } => self.quantum_param(*base),
            TyKind::Tuple(elems) => elems.iter().find_map(|&t| self.quantum_param(t)),
            TyKind::Array(elem, _) | TyKind::SizedArray(elem, _) | TyKind::Distribution(elem) => {
                self.quantum_param(*elem)
            }
            TyKind::Ref { inner, .. } => self.quantum_param(*inner),
            TyKind::Struct(id) => self.structs[id.index()]
                .args
                .iter()
                .find_map(|&t| self.quantum_param(t)),
            _ => None,
        }
    }

    /// Matches `ty` against `pattern`, which can mention the type parameters `params`,
    /// recording what each stands for in `found`. Returns whether they match: an `impl` for
    /// `Pair<T, T>` matches `Pair<Int, Int>` with `T = Int`.
    pub fn match_params(
        &self,
        pattern: Ty,
        ty: Ty,
        params: &[Ty],
        found: &mut [Option<Ty>],
    ) -> bool {
        if let Some(i) = params.iter().position(|&p| p == pattern) {
            return match found[i] {
                None => {
                    found[i] = Some(ty);
                    true
                }
                Some(earlier) => earlier == ty,
            };
        }
        // The same type matches, but its parameters must still be bound, to themselves: an
        // `impl` for `Reg<N>` matched against `Reg<N>` in its own body.
        if pattern == ty && !self.has_params(pattern) {
            return true;
        }
        match (self.kind(pattern), self.kind(ty)) {
            (TyKind::Tuple(a), TyKind::Tuple(b)) if a.len() == b.len() => a
                .iter()
                .zip(b.iter())
                .all(|(&p, &t)| self.match_params(p, t, params, found)),
            (TyKind::Array(p, n), TyKind::Array(t, m)) if n == m => {
                self.match_params(*p, *t, params, found)
            }
            (TyKind::SizedArray(p, n), TyKind::SizedArray(t, m)) => {
                self.match_params(*p, *t, params, found) && self.match_params(*n, *m, params, found)
            }
            (&TyKind::SizedArray(p, n), &TyKind::Array(t, m)) => {
                let m = self
                    .find(&TyKind::Size(m))
                    .expect("interned with the array");
                self.match_params(p, t, params, found) && self.match_params(n, m, params, found)
            }
            (TyKind::Distribution(p), TyKind::Distribution(t)) => {
                self.match_params(*p, *t, params, found)
            }
            (
                TyKind::Ref {
                    mutable: a,
                    inner: p,
                },
                TyKind::Ref {
                    mutable: b,
                    inner: t,
                },
            ) if a == b => self.match_params(*p, *t, params, found),
            (
                TyKind::Assoc {
                    base: p,
                    trait_index: a,
                    args: pa,
                    name: n,
                    ..
                },
                TyKind::Assoc {
                    base: t,
                    trait_index: b,
                    args: ta,
                    name: m,
                    ..
                },
            ) if a == b && n == m && pa.len() == ta.len() => {
                self.match_params(*p, *t, params, found)
                    && pa
                        .iter()
                        .zip(ta.iter())
                        .all(|(&p, &t)| self.match_params(p, t, params, found))
            }
            (TyKind::Struct(a), TyKind::Struct(b)) => {
                let (da, db) = (&self.structs[a.index()], &self.structs[b.index()]);
                da.template.unwrap_or(*a) == db.template.unwrap_or(*b)
                    && da.args.len() == db.args.len()
                    && da
                        .args
                        .iter()
                        .zip(&db.args)
                        .all(|(&p, &t)| self.match_params(p, t, params, found))
            }
            _ => false,
        }
    }

    /// Whether `ty` mentions a type parameter.
    pub fn has_params(&self, ty: Ty) -> bool {
        match self.kind(ty) {
            TyKind::Param(_) => true,
            TyKind::Assoc { base, args, .. } => {
                self.has_params(*base) || args.iter().any(|&a| self.has_params(a))
            }
            TyKind::Tuple(elems) => elems.iter().any(|&t| self.has_params(t)),
            TyKind::Array(elem, _) | TyKind::Distribution(elem) => self.has_params(*elem),
            TyKind::SizedArray(..) | TyKind::SizeSum { .. } => true,
            TyKind::Ref { inner, .. } => self.has_params(*inner),
            TyKind::Struct(id) => self.structs[id.index()]
                .args
                .iter()
                .any(|&t| self.has_params(t)),
            _ => false,
        }
    }

    /// The type a reference refers to, or `ty` itself if it is not a reference.
    pub fn deref(&self, ty: Ty) -> Ty {
        match *self.kind(ty) {
            TyKind::Ref { inner, .. } => inner,
            _ => ty,
        }
    }

    /// Whether `ty` is the error type or `Never`, which are compatible with everything.
    pub fn is_wildcard(&self, ty: Ty) -> bool {
        ty == Ty::ERROR || ty == Ty::NEVER
    }

    /// Whether values of `ty` are copyable, affine or linear.
    pub fn linearity(&self, ty: Ty) -> Kind {
        self.linearity_at(ty, &[], 0)
    }

    /// [`Types::linearity`], with the type parameters `copy` taken to be `Copy`.
    pub fn linearity_assuming(&self, ty: Ty, copy: &[Ty]) -> Kind {
        self.linearity_at(ty, copy, 0)
    }

    /// The type parameters `ty` mentions, each once.
    pub fn params_in(&self, ty: Ty) -> Vec<Ty> {
        let mut found = Vec::new();
        self.collect_params(ty, &mut found);
        found
    }

    fn collect_params(&self, ty: Ty, found: &mut Vec<Ty>) {
        match self.kind(ty) {
            TyKind::Param(_) => {
                if !found.contains(&ty) {
                    found.push(ty);
                }
            }
            TyKind::Tuple(elems) => {
                for &e in elems.iter() {
                    self.collect_params(e, found);
                }
            }
            &TyKind::Array(elem, _) | &TyKind::Distribution(elem) => {
                self.collect_params(elem, found)
            }
            &TyKind::SizedArray(elem, len) => {
                self.collect_params(elem, found);
                self.collect_params(len, found);
            }
            TyKind::SizeSum { terms, .. } => {
                for &(p, _) in terms.iter() {
                    self.collect_params(p, found);
                }
            }
            &TyKind::Ref { inner, .. } => self.collect_params(inner, found),
            TyKind::Assoc { base, args, .. } => {
                self.collect_params(*base, found);
                for &a in args.iter() {
                    self.collect_params(a, found);
                }
            }
            &TyKind::Struct(id) => {
                for &a in &self.structs[id.index()].args {
                    self.collect_params(a, found);
                }
            }
            _ => {}
        }
    }

    fn linearity_at(&self, ty: Ty, copy: &[Ty], depth: usize) -> Kind {
        // A struct containing itself is reported elsewhere; stop rather than recurse forever.
        if depth > 64 {
            return Kind::Copyable;
        }
        let combine = |types: &mut dyn Iterator<Item = Ty>| {
            types
                .map(|t| self.linearity_at(t, copy, depth + 1))
                .max()
                .unwrap_or(Kind::Copyable)
        };
        match self.kind(ty) {
            TyKind::Qubit => Kind::Linear,
            // Generic code treats a value as a qubit unless it is known to be `Copy`.
            TyKind::Param(p) if self.params[p.index()].copy || copy.contains(&ty) => Kind::Copyable,
            TyKind::Param(_) => Kind::Linear,
            TyKind::Assoc { copy: true, .. } => Kind::Copyable,
            TyKind::Assoc { .. } => Kind::Linear,
            TyKind::Ref { mutable: true, .. } => Kind::Affine,
            TyKind::Tuple(elems) => combine(&mut elems.iter().copied()),
            TyKind::Array(elem, _) | TyKind::SizedArray(elem, _) => {
                self.linearity_at(*elem, copy, depth + 1)
            }
            TyKind::Struct(id) => {
                let def = &self.structs[id.index()];
                let variants = def.variants.iter().flatten().flat_map(|v| v.data.iter());
                combine(&mut def.fields.iter().map(|(_, t)| *t).chain(variants.copied()))
            }
            _ => Kind::Copyable,
        }
    }

    /// Whether `ty` contains qubits and no classical data (`spec/ownership.md` §1).
    pub fn is_pure_quantum(&self, ty: Ty) -> bool {
        self.pure_quantum_at(ty, 0)
    }

    fn pure_quantum_at(&self, ty: Ty, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        match self.kind(ty) {
            TyKind::Qubit => true,
            TyKind::Array(elem, len) => *len > 0 && self.pure_quantum_at(*elem, depth + 1),
            TyKind::SizedArray(elem, _) => self.pure_quantum_at(*elem, depth + 1),
            TyKind::Tuple(elems) => {
                !elems.is_empty() && elems.iter().all(|&t| self.pure_quantum_at(t, depth + 1))
            }
            TyKind::Struct(id) => {
                let fields = &self.structs[id.index()].fields;
                !fields.is_empty()
                    && fields
                        .iter()
                        .all(|&(_, t)| self.pure_quantum_at(t, depth + 1))
            }
            _ => false,
        }
    }

    /// Whether values of `ty` can be printed and interpolated into strings: classical
    /// values, through any number of shared references (`spec/types.md` §7).
    pub fn is_printable(&self, ty: Ty) -> bool {
        let ty = self.deref(ty);
        self.linearity(ty) == Kind::Copyable
            && !matches!(self.kind(ty), TyKind::Ref { .. })
            && !self.has_params(ty)
    }

    /// What the built-in operator `op` gives on `lhs` and `rhs`, if it applies.
    pub fn builtin_binary(&self, op: BinaryOp, lhs: Ty, rhs: Ty) -> Option<Ty> {
        use BinaryOp::*;
        match op {
            Or | And => (lhs == Ty::BOOL && rhs == Ty::BOOL).then_some(Ty::BOOL),
            Eq | NotEq => (lhs == rhs && self.is_comparable(lhs)).then_some(Ty::BOOL),
            Lt | Le | Gt | Ge => (lhs == rhs && lhs.is_number()).then_some(Ty::BOOL),
            BitOr | BitXor | BitAnd => {
                (lhs == rhs && (lhs.is_integer() || lhs == Ty::BIT)).then_some(lhs)
            }
            // The shift amount is the same type, or an `Int`.
            Shl | Shr => (lhs.is_integer() && (rhs == lhs || rhs == Ty::INT)).then_some(lhs),
            Add | Sub => match (lhs, rhs) {
                (a, b) if a == b && (a.is_number() || a.is_complex()) => Some(a),
                (Ty::OBSERVABLE, Ty::OBSERVABLE) => Some(Ty::OBSERVABLE),
                _ => None,
            },
            Mul => match (lhs, rhs) {
                (a, b) if a == b && (a.is_number() || a.is_complex()) => Some(a),
                (Ty::OBSERVABLE | Ty::FLOAT, Ty::OBSERVABLE) | (Ty::OBSERVABLE, Ty::FLOAT) => {
                    Some(Ty::OBSERVABLE)
                }
                _ => None,
            },
            Div => (lhs == rhs && (lhs.is_number() || lhs.is_complex())).then_some(lhs),
            Rem => (lhs == rhs && lhs.is_number()).then_some(lhs),
        }
    }

    /// What built-in unary `-` gives on `ty`, if it applies.
    pub fn builtin_neg(&self, ty: Ty) -> Option<Ty> {
        let ok = matches!(ty, Ty::INT | Ty::OBSERVABLE)
            || ty.is_float()
            || ty.is_complex()
            || ty.width().is_some_and(|w| w.signed);
        ok.then_some(ty)
    }

    /// The `std::ops` trait `trait_index` is, if it is one.
    pub fn op_trait(&self, trait_index: u32) -> Option<OpKind> {
        let i = self.ops.iter().position(|&t| t == Some(trait_index))?;
        Some(OPS[i].kind)
    }

    /// For a built-in type, what its built-in operator gives as the `std::ops` trait
    /// `trait_index` with the arguments `args` (any right side if `None`): the `Output`, or
    /// `()` for a compound assignment. `None` when the built-in operator does not apply, or
    /// the trait is not an operator's (`spec/types.md` §6).
    pub fn builtin_op(&self, trait_index: u32, self_ty: Ty, args: Option<&[Ty]>) -> Option<Ty> {
        if !self.is_builtin_value(self_ty) {
            return None;
        }
        match self.op_trait(trait_index)? {
            OpKind::Neg => self.builtin_neg(self_ty),
            OpKind::Binary(op) => {
                let rhs = match args {
                    Some(&[rhs]) => rhs,
                    Some(_) => return None,
                    None => self_ty,
                };
                self.builtin_binary(op, self_ty, rhs)
            }
            OpKind::Assign(op) => {
                let rhs = match args {
                    Some(&[rhs]) => rhs,
                    Some(_) => return None,
                    None => self_ty,
                };
                (self.builtin_binary(op, self_ty, rhs) == Some(self_ty)).then_some(Ty::UNIT)
            }
        }
    }

    /// Whether `ty` is a built-in type with operators of its own: a number, a complex
    /// number, `Bit`, `Bool` or `Observable`.
    fn is_builtin_value(&self, ty: Ty) -> bool {
        ty.is_number() || ty.is_complex() || matches!(ty, Ty::BIT | Ty::BOOL | Ty::OBSERVABLE)
    }

    /// Whether `==` and `!=` work on values of `ty`: numbers, logic, strings, and arrays,
    /// tuples and enums whose parts all work.
    pub fn is_comparable(&self, ty: Ty) -> bool {
        self.comparable_at(ty, 0)
    }

    fn comparable_at(&self, ty: Ty, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        match *self.kind(ty) {
            TyKind::Int | TyKind::Float | TyKind::Bool | TyKind::Bit | TyKind::String => true,
            _ if ty.is_number() || ty.is_complex() => true,
            // Element by element: `m == [0, 1]` for measured bits.
            TyKind::Array(elem, _) | TyKind::SizedArray(elem, _) => {
                self.comparable_at(elem, depth + 1)
            }
            TyKind::Tuple(ref elems) => elems.iter().all(|&t| self.comparable_at(t, depth + 1)),
            TyKind::Struct(id) => match &self.structs[id.index()].variants {
                Some(variants) => variants
                    .iter()
                    .all(|v| v.data.iter().all(|&t| self.comparable_at(t, depth + 1))),
                None => false,
            },
            _ => false,
        }
    }

    /// The variants of `ty`, if it is an enum.
    pub fn variants(&self, ty: Ty) -> Option<&[VariantDef]> {
        match *self.kind(ty) {
            TyKind::Struct(id) => self.structs[id.index()].variants.as_deref(),
            _ => None,
        }
    }

    /// How many qubits a pure quantum value of type `ty` holds, if it is pure quantum.
    pub fn qubit_count(&self, ty: Ty) -> Option<u64> {
        if !self.is_pure_quantum(ty) {
            return None;
        }
        Some(self.count_qubits(ty))
    }

    fn count_qubits(&self, ty: Ty) -> u64 {
        match self.kind(ty) {
            TyKind::Qubit => 1,
            TyKind::Array(elem, len) => len * self.count_qubits(*elem),
            TyKind::Tuple(elems) => elems.iter().map(|&t| self.count_qubits(t)).sum(),
            TyKind::Struct(id) => self.structs[id.index()]
                .fields
                .iter()
                .map(|&(_, t)| self.count_qubits(t))
                .sum(),
            _ => 0,
        }
    }

    /// How `ty` is written, for messages.
    pub fn display(&self, ty: Ty, ast: &Ast) -> String {
        match self.kind(ty) {
            TyKind::Error => "{unknown}".into(),
            TyKind::Never => "!".into(),
            TyKind::Unit => "()".into(),
            TyKind::Int => "Int".into(),
            TyKind::Float => "Float".into(),
            TyKind::Bool => "Bool".into(),
            TyKind::Bit => "Bit".into(),
            TyKind::Qubit => "Qubit".into(),
            TyKind::Observable => "Observable".into(),
            TyKind::String => "String".into(),
            TyKind::I8
            | TyKind::I16
            | TyKind::I32
            | TyKind::I64
            | TyKind::U8
            | TyKind::U16
            | TyKind::U32
            | TyKind::U64 => ty.width().expect("a sized integer").name(),
            TyKind::F32 => "f32".into(),
            TyKind::Complex => "Complex".into(),
            TyKind::C32 => "c32".into(),
            TyKind::Tuple(elems) => {
                let elems: Vec<String> = elems.iter().map(|&t| self.display(t, ast)).collect();
                if elems.len() == 1 {
                    format!("({},)", elems[0])
                } else {
                    format!("({})", elems.join(", "))
                }
            }
            TyKind::Array(elem, len) => format!("[{}; {len}]", self.display(*elem, ast)),
            TyKind::SizedArray(elem, len) => {
                format!(
                    "[{}; {}]",
                    self.display(*elem, ast),
                    self.display(*len, ast)
                )
            }
            TyKind::Size(n) => n.to_string(),
            TyKind::SizeSum { constant, terms } => {
                let mut parts: Vec<String> = terms
                    .iter()
                    .map(|&(p, k)| match k {
                        1 => self.display(p, ast),
                        _ => format!("{k} * {}", self.display(p, ast)),
                    })
                    .collect();
                if *constant != 0 {
                    parts.push(constant.to_string());
                }
                parts.join(" + ")
            }
            TyKind::Distribution(elem) => format!("Distribution<{}>", self.display(*elem, ast)),
            TyKind::Struct(id) => {
                let def = &self.structs[id.index()];
                let name = ast.name(def.name);
                if def.args.is_empty() {
                    return name.into();
                }
                let args: Vec<String> = def.args.iter().map(|&t| self.display(t, ast)).collect();
                format!("{name}<{}>", args.join(", "))
            }
            TyKind::Ref { mutable, inner } => {
                let word = if *mutable { "mut" } else { "ref" };
                format!("{word} {}", self.display(*inner, ast))
            }
            TyKind::Param(p) => ast.name(self.params[p.index()].name).into(),
            TyKind::Assoc { base, name, .. } => {
                format!("{}::{}", self.display(*base, ast), ast.name(*name))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_and_kinds() {
        let mut types = Types::default();
        let pair = types.intern(TyKind::Tuple([Ty::QUBIT, Ty::QUBIT].into()));
        assert_eq!(
            pair,
            types.intern(TyKind::Tuple([Ty::QUBIT, Ty::QUBIT].into()))
        );
        let mixed = types.intern(TyKind::Tuple([Ty::QUBIT, Ty::FLOAT].into()));
        let reg = types.array(Ty::QUBIT, 4);
        let lend = types.reference(true, Ty::QUBIT);

        assert_eq!(types.linearity(Ty::INT), Kind::Copyable);
        assert_eq!(types.linearity(mixed), Kind::Linear);
        assert_eq!(types.linearity(lend), Kind::Affine);
        assert!(types.is_pure_quantum(pair) && types.is_pure_quantum(reg));
        assert!(!types.is_pure_quantum(mixed) && !types.is_pure_quantum(lend));
        assert_eq!(types.qubit_count(reg), Some(4));
    }
}
