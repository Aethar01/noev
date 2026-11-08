//! Everything in scope without imports, and its signatures: the intrinsics of `spec/core.md`
//! and the standard library of `spec/std.md`.
//!
//! The standard library is implemented natively for now. Each of its items must keep exactly
//! the signature, ownership and reversibility of its reference definition in `spec/std.md`,
//! so it can later move to Noev source unnoticed.

use noev_syntax::ast::OwnKind;

use crate::ty::{Ty, TyKind, Types};

/// A built-in function, method or constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Builtin {
    /// A gate, as a method or a free function.
    Gate(Gate),
    /// `measure`, on a qubit or an array of qubits.
    Measure,
    /// `measure_in(basis)`, on a qubit or an array of qubits. Standard library: a rotation
    /// into the Z basis, then `measure`.
    MeasureIn,
    /// `reset`.
    Reset,
    /// `discard`. Standard library: `reset`, then `release`.
    Discard,
    /// `release` (unsafe).
    Release,
    /// `X(i)`, `Y(i)` or `Z(i)`.
    Pauli(Pauli),
    /// `I()`.
    Identity,
    /// `P(i, b)`: a projector in the Z basis. Standard library: a sum of `I()` and `Z(i)`.
    ProjectorZ,
    /// `PX(i, b)`: a projector in the X basis. Standard library: a sum of `I()` and `X(i)`.
    ProjectorX,
    /// A method on `Float` or `Int`.
    Math(Math),
    /// `Float(x)`, `Int(x)`, `Bit(x)` or `Bool(x)`.
    Convert(Ty),
    /// `Float::PI`, `Float::TAU` or `Float::E`.
    Const(FloatConst),
    /// A method of `Distribution<T>`.
    Dist(DistMethod),
    /// `print(…)` (`stderr: false`) or `eprint(…)` (`stderr: true`). Standard library.
    Print {
        /// Whether it writes to standard error.
        stderr: bool,
    },
}

impl Builtin {
    /// Whether it may be used in a reversible context.
    pub fn reversible(self) -> bool {
        !matches!(
            self,
            Builtin::Measure
                | Builtin::MeasureIn
                | Builtin::Reset
                | Builtin::Discard
                | Builtin::Release
                | Builtin::Print { .. }
        )
    }

    /// Whether calling it needs `unsafe`.
    pub fn is_unsafe(self) -> bool {
        self == Builtin::Release
    }

    /// The name it is called by, for messages.
    pub fn name(self) -> &'static str {
        match self {
            Builtin::Measure => "measure",
            Builtin::MeasureIn => "measure_in",
            Builtin::Reset => "reset",
            Builtin::Discard => "discard",
            Builtin::Release => "release",
            Builtin::Gate(_) => "a gate",
            Builtin::Pauli(_) | Builtin::Identity => "an observable",
            Builtin::ProjectorZ => "P",
            Builtin::ProjectorX => "PX",
            Builtin::Math(_) => "a maths function",
            Builtin::Convert(_) => "a conversion",
            Builtin::Const(_) => "a constant",
            Builtin::Dist(_) => "a distribution method",
            Builtin::Print { stderr: false } => "print",
            Builtin::Print { stderr: true } => "eprint",
        }
    }
}

/// The methods of `Distribution<T>` (`spec/execution.md` §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DistMethod {
    /// `shots() -> Int`
    Shots,
    /// `count(x: T) -> Int`
    Count,
    /// `probability(x: T) -> Float`
    Probability,
    /// `most_likely() -> T`
    MostLikely,
}

/// The prelude's gates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(missing_docs)] // Each variant is named for its gate.
pub enum Gate {
    X,
    Y,
    Z,
    H,
    S,
    T,
    Rx,
    Ry,
    Rz,
    P,
    U,
    Swap,
    /// `unitary(m)`: a one- or two-qubit gate given as a matrix of `Complex` values. Its
    /// entries travel as the gate's parameters: real and imaginary parts, rows first.
    Unitary,
}

impl Gate {
    fn from_name(name: &str) -> Option<Gate> {
        use Gate::*;
        Some(match name {
            "x" => X,
            "y" => Y,
            "z" => Z,
            "h" => H,
            "s" => S,
            "t" => T,
            "rx" => Rx,
            "ry" => Ry,
            "rz" => Rz,
            "p" => P,
            "u" => U,
            "swap" => Swap,
            _ => return None,
        })
    }

    /// The gate's parameters after the target qubit.
    fn params(self, types: &mut Types) -> Vec<Ty> {
        use Gate::*;
        match self {
            X | Y | Z | H | S | T => vec![],
            Rx | Ry | Rz | P => vec![Ty::FLOAT],
            U => vec![Ty::FLOAT; 3],
            Swap => vec![types.reference(true, Ty::QUBIT)],
            Unitary => vec![matrix(types, 2)],
        }
    }
}

/// The type of an `n`×`n` matrix of `Complex` values, `[[Complex; n]; n]`.
fn matrix(types: &mut Types, n: u64) -> Ty {
    let row = types.array(Ty::COMPLEX, n);
    types.array(row, n)
}

/// A Pauli operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum Pauli {
    X,
    Y,
    Z,
}

/// A maths method.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(missing_docs)] // Each variant is named for its method.
pub enum Math {
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Atan2,
    Sqrt,
    Exp,
    Ln,
    Abs,
    Min,
    Max,
    Trunc,
    Round,
    Floor,
    Ceil,
    /// `x.bits()` on an unsigned integer: its bits, most significant first.
    Bits,
    /// `z.re()`: a complex number's real part.
    Re,
    /// `z.im()`: its imaginary part.
    Im,
    /// `z.arg()`: its phase, in (−π, π].
    Arg,
    /// `z.conj()`: its complex conjugate.
    Conj,
    /// `Complex::polar(r, theta)`: the complex number with modulus `r` and phase `theta`.
    Polar,
}

/// A `Float` constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum FloatConst {
    Pi,
    Tau,
    E,
}

/// Whether calling something is reversible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reversible {
    /// Always reversible: gates, `rev fn`s, pure classical built-ins.
    Yes,
    /// Never reversible: measurement, reset.
    No,
    /// A user function without `rev`, whose reversibility is inferred from its body.
    Infer,
}

/// A function's signature.
#[derive(Clone, Debug, PartialEq)]
pub struct Sig {
    /// How a method takes `self`; `None` for functions.
    pub receiver: Option<OwnKind>,
    /// The parameter types, after the receiver.
    pub params: Vec<Ty>,
    /// The return type.
    pub ret: Ty,
    /// Whether calling it needs `unsafe`.
    pub is_unsafe: bool,
    /// Whether it may be used in a reversible context.
    pub reversible: Reversible,
}

impl Sig {
    fn new(receiver: Option<OwnKind>, params: Vec<Ty>, ret: Ty, reversible: Reversible) -> Self {
        Sig {
            receiver,
            params,
            ret,
            is_unsafe: false,
            reversible,
        }
    }
}

/// The names of the built-in types.
pub fn builtin_type(name: &str) -> Option<Ty> {
    Some(match name {
        "Int" => Ty::INT,
        "Float" => Ty::FLOAT,
        "Bool" => Ty::BOOL,
        "Bit" => Ty::BIT,
        "Qubit" => Ty::QUBIT,
        "Observable" => Ty::OBSERVABLE,
        "String" => Ty::STRING,
        "i8" => Ty::I8,
        "i16" => Ty::I16,
        "i32" => Ty::I32,
        "i64" => Ty::I64,
        "u8" => Ty::U8,
        "u16" => Ty::U16,
        "u32" => Ty::U32,
        "u64" => Ty::U64,
        "f32" => Ty::F32,
        // `Complex` is two `Float`s, as Python's `complex` is.
        "Complex" | "c64" => Ty::COMPLEX,
        "c32" => Ty::C32,
        // `Float` is a 64-bit IEEE 754 float, as Python's `float` is.
        "f64" => Ty::FLOAT,
        _ => return None,
    })
}

/// A built-in method on a value whose type (after following references) is `base`.
pub fn method(types: &mut Types, base: Ty, name: &str) -> Option<(Builtin, Sig)> {
    use Reversible::*;
    let self_mut = types.reference(true, base);
    match types.kind(base).clone() {
        TyKind::Qubit => {
            let gate = match name {
                "unitary" => Some(Gate::Unitary),
                _ => Gate::from_name(name),
            };
            if let Some(gate) = gate {
                let params = gate.params(types);
                let sig = Sig::new(Some(OwnKind::Mut), params, self_mut, Yes);
                return Some((Builtin::Gate(gate), sig));
            }
            let (builtin, sig) = match name {
                "measure" => (
                    Builtin::Measure,
                    Sig::new(Some(OwnKind::Mv), vec![], Ty::BIT, No),
                ),
                "measure_in" => (
                    Builtin::MeasureIn,
                    Sig::new(Some(OwnKind::Mv), vec![types.basis?], Ty::BIT, No),
                ),
                "reset" => (
                    Builtin::Reset,
                    Sig::new(Some(OwnKind::Mut), vec![], self_mut, No),
                ),
                "discard" => (
                    Builtin::Discard,
                    Sig::new(Some(OwnKind::Mv), vec![], Ty::UNIT, No),
                ),
                "release" => {
                    let mut sig = Sig::new(Some(OwnKind::Mv), vec![], Ty::UNIT, No);
                    sig.is_unsafe = true;
                    (Builtin::Release, sig)
                }
                _ => return None,
            };
            Some((builtin, sig))
        }
        // A two-qubit gate from its matrix.
        TyKind::Array(Ty::QUBIT, 2) if name == "unitary" => {
            let params = vec![matrix(types, 4)];
            Some((
                Builtin::Gate(Gate::Unitary),
                Sig::new(Some(OwnKind::Mut), params, self_mut, Yes),
            ))
        }
        TyKind::Array(elem, len) if elem == Ty::QUBIT && name == "measure" => {
            let bits = types.array(Ty::BIT, len);
            Some((
                Builtin::Measure,
                Sig::new(Some(OwnKind::Mv), vec![], bits, No),
            ))
        }
        TyKind::Array(elem, len) if elem == Ty::QUBIT && name == "measure_in" => {
            let bits = types.array(Ty::BIT, len);
            Some((
                Builtin::MeasureIn,
                Sig::new(Some(OwnKind::Mv), vec![types.basis?], bits, No),
            ))
        }
        // `Float` and `f32` have the same methods, each on its own type.
        TyKind::Float | TyKind::F32 => {
            use Math::*;
            let f = base;
            let (math, params, ret) = match name {
                "sin" => (Sin, vec![], f),
                "cos" => (Cos, vec![], f),
                "tan" => (Tan, vec![], f),
                "asin" => (Asin, vec![], f),
                "acos" => (Acos, vec![], f),
                "atan" => (Atan, vec![], f),
                "atan2" => (Atan2, vec![f], f),
                "sqrt" => (Sqrt, vec![], f),
                "exp" => (Exp, vec![], f),
                "ln" => (Ln, vec![], f),
                "abs" => (Abs, vec![], f),
                "min" => (Min, vec![f], f),
                "max" => (Max, vec![f], f),
                "trunc" => (Trunc, vec![], Ty::INT),
                "round" => (Round, vec![], Ty::INT),
                "floor" => (Floor, vec![], Ty::INT),
                "ceil" => (Ceil, vec![], Ty::INT),
                _ => return None,
            };
            Some((
                Builtin::Math(math),
                Sig::new(Some(OwnKind::Ref), params, ret, Yes),
            ))
        }
        TyKind::Distribution(elem) => {
            let (method, params, ret) = match name {
                "shots" => (DistMethod::Shots, vec![], Ty::INT),
                "count" => (DistMethod::Count, vec![elem], Ty::INT),
                "probability" => (DistMethod::Probability, vec![elem], Ty::FLOAT),
                "most_likely" => (DistMethod::MostLikely, vec![], elem),
                _ => return None,
            };
            Some((
                Builtin::Dist(method),
                Sig::new(Some(OwnKind::Ref), params, ret, Yes),
            ))
        }
        // `Complex` and `c32`, whose parts are `Float`s and `f32`s.
        TyKind::Complex | TyKind::C32 => {
            use Math::*;
            let part = if base == Ty::C32 { Ty::F32 } else { Ty::FLOAT };
            let receiver = Some(OwnKind::Ref);
            let (math, receiver, params, ret) = match name {
                "re" => (Re, receiver, vec![], part),
                "im" => (Im, receiver, vec![], part),
                "abs" => (Abs, receiver, vec![], part),
                "arg" => (Arg, receiver, vec![], part),
                "conj" => (Conj, receiver, vec![], base),
                "exp" => (Exp, receiver, vec![], base),
                "sqrt" => (Sqrt, receiver, vec![], base),
                "polar" => (Polar, None, vec![part, part], base),
                _ => return None,
            };
            Some((Builtin::Math(math), Sig::new(receiver, params, ret, Yes)))
        }
        _ if base.is_integer() => {
            let (math, params, ret) = match name {
                "abs" if base.width().is_none_or(|w| w.signed) => (Math::Abs, vec![], base),
                "min" => (Math::Min, vec![base], base),
                "max" => (Math::Max, vec![base], base),
                "bits" if base.width().is_some_and(|w| !w.signed) => {
                    let bits = base.width().expect("checked").bits;
                    (Math::Bits, vec![], types.array(Ty::BIT, u64::from(bits)))
                }
                _ => return None,
            };
            Some((
                Builtin::Math(math),
                Sig::new(Some(OwnKind::Ref), params, ret, Yes),
            ))
        }
        _ => None,
    }
}

/// A built-in free function. `measure` is overloaded; its parameter here is `Qubit`, and the
/// checker handles arrays.
pub fn function(types: &mut Types, name: &str) -> Option<(Builtin, Sig)> {
    use Reversible::*;
    let qubit_mut = types.reference(true, Ty::QUBIT);
    if let Some(gate) = Gate::from_name(name) {
        let mut params = vec![qubit_mut];
        params.extend(gate.params(types));
        return Some((Builtin::Gate(gate), Sig::new(None, params, Ty::UNIT, Yes)));
    }
    let (builtin, sig) = match name {
        "measure" => (
            Builtin::Measure,
            Sig::new(None, vec![Ty::QUBIT], Ty::BIT, No),
        ),
        "measure_in" => (
            Builtin::MeasureIn,
            Sig::new(None, vec![Ty::QUBIT, types.basis?], Ty::BIT, No),
        ),
        "reset" => (
            Builtin::Reset,
            Sig::new(None, vec![qubit_mut], Ty::UNIT, No),
        ),
        "discard" => (
            Builtin::Discard,
            Sig::new(None, vec![Ty::QUBIT], Ty::UNIT, No),
        ),
        "X" | "Y" | "Z" => {
            let pauli = match name {
                "X" => Pauli::X,
                "Y" => Pauli::Y,
                _ => Pauli::Z,
            };
            let sig = Sig::new(None, vec![Ty::INT], Ty::OBSERVABLE, Yes);
            (Builtin::Pauli(pauli), sig)
        }
        "I" => (
            Builtin::Identity,
            Sig::new(None, vec![], Ty::OBSERVABLE, Yes),
        ),
        "P" | "PX" => {
            let builtin = if name == "P" {
                Builtin::ProjectorZ
            } else {
                Builtin::ProjectorX
            };
            let sig = Sig::new(None, vec![Ty::INT, Ty::BIT], Ty::OBSERVABLE, Yes);
            (builtin, sig)
        }
        _ => return None,
    };
    Some((builtin, sig))
}

/// A built-in associated constant, such as `Float::PI`.
pub fn constant(ty: Ty, name: &str) -> Option<Builtin> {
    if ty != Ty::FLOAT {
        return None;
    }
    Some(Builtin::Const(match name {
        "PI" => FloatConst::Pi,
        "TAU" => FloatConst::Tau,
        "E" => FloatConst::E,
        _ => return None,
    }))
}

/// Whether `target(x)` converts a value of type `from` (`spec/types.md` §4).
pub fn converts(types: &Types, from: Ty, target: Ty) -> bool {
    let logic = matches!(from, Ty::BIT | Ty::BOOL);
    let exact_int = match (from.width(), target.width()) {
        (Some(a), Some(b)) => a.fits_in(b),
        (Some(_) | None, None) => target == Ty::INT && (from.is_integer() || logic),
        (None, Some(_)) => logic,
    };
    // A measured register as a number: `u8(bits)` from a `[Bit; 8]`.
    let bits = match (types.kind(from), target.width()) {
        (&TyKind::Array(Ty::BIT, len), Some(w)) => !w.signed && len == u64::from(w.bits),
        _ => false,
    };
    from == target
        || exact_int
        || bits
        || (target.is_float() && (from.is_number()))
        || (target.is_complex() && (from.is_number() || from.is_complex()))
        || matches!((from, target), (Ty::BIT, Ty::BOOL) | (Ty::BOOL, Ty::BIT))
}
