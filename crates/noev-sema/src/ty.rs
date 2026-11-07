//! Types, interned so that comparing two types is comparing two integers.

use noev_syntax::{Ast, FxHashMap, Span, Symbol};

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
    /// `(A, B, …)`.
    Tuple(Box<[Ty]>),
    /// `[T; N]`.
    Array(Ty, u64),
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
    /// The program's structs.
    pub structs: Vec<StructDef>,
}

macro_rules! basic {
    ($($name:ident = $index:literal: $kind:ident,)*) => {
        impl Ty {
            $(
                #[allow(missing_docs)]
                pub const $name: Ty = Ty($index);
            )*
        }

        impl Default for Types {
            fn default() -> Self {
                let mut types = Types {
                    kinds: Vec::new(),
                    map: FxHashMap::default(),
                    structs: Vec::new(),
                };
                $(
                    let ty = types.intern(TyKind::$kind);
                    debug_assert_eq!(ty, Ty::$name);
                )*
                types
            }
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
}

impl Types {
    /// The type for `kind`, creating it if needed.
    pub fn intern(&mut self, kind: TyKind) -> Ty {
        if let Some(&ty) = self.map.get(&kind) {
            return ty;
        }
        let ty = Ty(u32::try_from(self.kinds.len()).expect("fewer than 2³² types"));
        self.kinds.push(kind.clone());
        self.map.insert(kind, ty);
        ty
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
        self.linearity_at(ty, 0)
    }

    fn linearity_at(&self, ty: Ty, depth: usize) -> Kind {
        // A struct containing itself is reported elsewhere; stop rather than recurse forever.
        if depth > 64 {
            return Kind::Copyable;
        }
        let combine = |types: &mut dyn Iterator<Item = Ty>| {
            types
                .map(|t| self.linearity_at(t, depth + 1))
                .max()
                .unwrap_or(Kind::Copyable)
        };
        match self.kind(ty) {
            TyKind::Qubit => Kind::Linear,
            TyKind::Ref { mutable: true, .. } => Kind::Affine,
            TyKind::Tuple(elems) => combine(&mut elems.iter().copied()),
            TyKind::Array(elem, _) => self.linearity_at(*elem, depth + 1),
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
        self.linearity(ty) == Kind::Copyable && !matches!(self.kind(ty), TyKind::Ref { .. })
    }

    /// Whether `==` and `!=` work on values of `ty`: numbers, logic, strings, and enums whose
    /// data all work.
    pub fn is_comparable(&self, ty: Ty) -> bool {
        self.comparable_at(ty, 0)
    }

    fn comparable_at(&self, ty: Ty, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        match *self.kind(ty) {
            TyKind::Int | TyKind::Float | TyKind::Bool | TyKind::Bit | TyKind::String => true,
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
            TyKind::Tuple(elems) => {
                let elems: Vec<String> = elems.iter().map(|&t| self.display(t, ast)).collect();
                if elems.len() == 1 {
                    format!("({},)", elems[0])
                } else {
                    format!("({})", elems.join(", "))
                }
            }
            TyKind::Array(elem, len) => format!("[{}; {len}]", self.display(*elem, ast)),
            TyKind::Distribution(elem) => format!("Distribution<{}>", self.display(*elem, ast)),
            TyKind::Struct(id) => ast.name(self.structs[id.index()].name).into(),
            TyKind::Ref { mutable, inner } => {
                let word = if *mutable { "mut" } else { "ref" };
                format!("{word} {}", self.display(*inner, ast))
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
