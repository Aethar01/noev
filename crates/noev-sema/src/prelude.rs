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
        }
    }
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
        _ => return None,
    })
}

/// A built-in method on a value whose type (after following references) is `base`.
pub fn method(types: &mut Types, base: Ty, name: &str) -> Option<(Builtin, Sig)> {
    use Reversible::*;
    let self_mut = types.reference(true, base);
    match types.kind(base).clone() {
        TyKind::Qubit => {
            if let Some(gate) = Gate::from_name(name) {
                let params = gate.params(types);
                let sig = Sig::new(Some(OwnKind::Mut), params, self_mut, Yes);
                return Some((Builtin::Gate(gate), sig));
            }
            let (builtin, sig) = match name {
                "measure" => (
                    Builtin::Measure,
                    Sig::new(Some(OwnKind::Mv), vec![], Ty::BIT, No),
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
        TyKind::Array(elem, len) if elem == Ty::QUBIT && name == "measure" => {
            let bits = types.array(Ty::BIT, len);
            Some((
                Builtin::Measure,
                Sig::new(Some(OwnKind::Mv), vec![], bits, No),
            ))
        }
        TyKind::Float => {
            use Math::*;
            let (math, params, ret) = match name {
                "sin" => (Sin, vec![], Ty::FLOAT),
                "cos" => (Cos, vec![], Ty::FLOAT),
                "tan" => (Tan, vec![], Ty::FLOAT),
                "asin" => (Asin, vec![], Ty::FLOAT),
                "acos" => (Acos, vec![], Ty::FLOAT),
                "atan" => (Atan, vec![], Ty::FLOAT),
                "atan2" => (Atan2, vec![Ty::FLOAT], Ty::FLOAT),
                "sqrt" => (Sqrt, vec![], Ty::FLOAT),
                "exp" => (Exp, vec![], Ty::FLOAT),
                "ln" => (Ln, vec![], Ty::FLOAT),
                "abs" => (Abs, vec![], Ty::FLOAT),
                "min" => (Min, vec![Ty::FLOAT], Ty::FLOAT),
                "max" => (Max, vec![Ty::FLOAT], Ty::FLOAT),
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
        TyKind::Int => {
            let (math, params) = match name {
                "abs" => (Math::Abs, vec![]),
                "min" => (Math::Min, vec![Ty::INT]),
                "max" => (Math::Max, vec![Ty::INT]),
                _ => return None,
            };
            Some((
                Builtin::Math(math),
                Sig::new(Some(OwnKind::Ref), params, Ty::INT, Yes),
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
pub fn converts(from: Ty, target: Ty) -> bool {
    from == target
        || matches!(
            (from, target),
            (Ty::INT, Ty::FLOAT)
                | (Ty::BIT, Ty::BOOL)
                | (Ty::BOOL, Ty::BIT)
                | (Ty::BIT, Ty::INT)
                | (Ty::BOOL, Ty::INT)
        )
}
