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
    /// `amplitudes(r)`: a register's amplitudes, on a simulator (`spec/std.md` §2).
    Amplitudes,
    /// `print(…)` (`stderr: false`) or `eprint(…)` (`stderr: true`). Standard library.
    Print {
        /// Whether it writes to standard error.
        stderr: bool,
    },
    /// `qasm("…")`: inline OpenQASM (unsafe; `spec/interop.md` §4).
    Qasm,
    /// Standard-library classical/OS operation (`Vec<T>`, `String`, `std::{io, env, fs, net}`).
    Std(StdOp),
}

/// Built-in standard-library operations (`spec/std.md` §5–§6).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum StdOp {
    // Vec<T>
    VecNew = 0,
    VecWithCapacity = 1,
    VecLen = 2,
    VecIsEmpty = 3,
    VecPush = 4,
    VecPop = 5,
    VecGet = 6,
    VecIndex = 7,
    VecSet = 8,
    VecClear = 9,
    VecExtend = 10,
    VecClone = 11,
    // String
    StrLen = 20,
    StrIsEmpty = 21,
    StrContains = 22,
    StrStartsWith = 23,
    StrEndsWith = 24,
    StrTrim = 25,
    StrSplit = 26,
    StrLines = 27,
    StrReplace = 28,
    StrSubstring = 29,
    StrFind = 30,
    StrToUpper = 31,
    StrToLower = 32,
    StrAsBytes = 33,
    StrFromUtf8 = 34,
    StrParseInt = 35,
    StrParseFloat = 36,
    StrRepeat = 37,
    StrJoin = 38,
    StrConcat = 39,
    // std::io
    IoStdout = 40,
    IoStderr = 41,
    IoStdin = 42,
    IoLock = 43,
    IoWrite = 44,
    IoFlush = 45,
    IoReadLine = 46,
    IoReadToString = 47,
    // std::env
    EnvVar = 50,
    EnvArgs = 51,
    EnvCurrentDir = 52,
    // std::fs
    FsReadToString = 60,
    FsRead = 61,
    FsWrite = 62,
    FsWriteBytes = 63,
    FsExists = 64,
    FsCreateDirAll = 65,
    FsRemoveFile = 66,
    // std::net
    NetTcpConnect = 70,
    NetTcpRead = 71,
    NetTcpReadToString = 72,
    NetTcpWrite = 73,
    NetTcpWriteStr = 74,
    NetTcpFlush = 75,
    NetTcpShutdown = 76,
    NetTcpClose = 77,
    NetListenerBind = 78,
    NetListenerLocalAddr = 79,
    NetListenerAccept = 80,
    NetListenerClose = 81,
}

impl StdOp {
    /// Decodes a `u32` tag from compiled C ABI calls.
    pub fn from_u32(v: u32) -> Option<Self> {
        use StdOp::*;
        Some(match v {
            0 => VecNew,
            1 => VecWithCapacity,
            2 => VecLen,
            3 => VecIsEmpty,
            4 => VecPush,
            5 => VecPop,
            6 => VecGet,
            7 => VecIndex,
            8 => VecSet,
            9 => VecClear,
            10 => VecExtend,
            11 => VecClone,
            20 => StrLen,
            21 => StrIsEmpty,
            22 => StrContains,
            23 => StrStartsWith,
            24 => StrEndsWith,
            25 => StrTrim,
            26 => StrSplit,
            27 => StrLines,
            28 => StrReplace,
            29 => StrSubstring,
            30 => StrFind,
            31 => StrToUpper,
            32 => StrToLower,
            33 => StrAsBytes,
            34 => StrFromUtf8,
            35 => StrParseInt,
            36 => StrParseFloat,
            37 => StrRepeat,
            38 => StrJoin,
            39 => StrConcat,
            40 => IoStdout,
            41 => IoStderr,
            42 => IoStdin,
            43 => IoLock,
            44 => IoWrite,
            45 => IoFlush,
            46 => IoReadLine,
            47 => IoReadToString,
            50 => EnvVar,
            51 => EnvArgs,
            52 => EnvCurrentDir,
            60 => FsReadToString,
            61 => FsRead,
            62 => FsWrite,
            63 => FsWriteBytes,
            64 => FsExists,
            65 => FsCreateDirAll,
            66 => FsRemoveFile,
            70 => NetTcpConnect,
            71 => NetTcpRead,
            72 => NetTcpReadToString,
            73 => NetTcpWrite,
            74 => NetTcpWriteStr,
            75 => NetTcpFlush,
            76 => NetTcpShutdown,
            77 => NetTcpClose,
            78 => NetListenerBind,
            79 => NetListenerLocalAddr,
            80 => NetListenerAccept,
            81 => NetListenerClose,
            _ => return None,
        })
    }

    /// Whether the operation is pure classical (allowed in reversible contexts).
    pub fn reversible(self) -> bool {
        (self as u32) < 40
    }

    /// Whether this operation performs OS/IO side effects (disallowed in recorded circuits).
    pub fn is_effectful(self) -> bool {
        !self.reversible()
    }

    /// Short display name for IR and diagnostics.
    pub fn name(self) -> &'static str {
        use StdOp::*;
        match self {
            VecNew => "Vec::new",
            VecWithCapacity => "Vec::with_capacity",
            VecLen => "Vec::len",
            VecIsEmpty => "Vec::is_empty",
            VecPush => "Vec::push",
            VecPop => "Vec::pop",
            VecGet => "Vec::get",
            VecIndex => "Vec::index",
            VecSet => "Vec::set",
            VecClear => "Vec::clear",
            VecExtend => "Vec::extend",
            VecClone => "Vec::clone",
            StrLen => "String::len",
            StrIsEmpty => "String::is_empty",
            StrContains => "String::contains",
            StrStartsWith => "String::starts_with",
            StrEndsWith => "String::ends_with",
            StrTrim => "String::trim",
            StrSplit => "String::split",
            StrLines => "String::lines",
            StrReplace => "String::replace",
            StrSubstring => "String::substring",
            StrFind => "String::find",
            StrToUpper => "String::to_uppercase",
            StrToLower => "String::to_lowercase",
            StrAsBytes => "String::as_bytes",
            StrFromUtf8 => "String::from_utf8",
            StrParseInt => "String::parse_int",
            StrParseFloat => "String::parse_float",
            StrRepeat => "String::repeat",
            StrJoin => "String::join",
            StrConcat => "String::concat",
            IoStdout => "std::io::stdout",
            IoStderr => "std::io::stderr",
            IoStdin => "std::io::stdin",
            IoLock => "IoStream::lock",
            IoWrite => "IoStream::write",
            IoFlush => "IoStream::flush",
            IoReadLine => "Stdin::read_line",
            IoReadToString => "Stdin::read_to_string",
            EnvVar => "std::env::var",
            EnvArgs => "std::env::args",
            EnvCurrentDir => "std::env::current_dir",
            FsReadToString => "std::fs::read_to_string",
            FsRead => "std::fs::read",
            FsWrite => "std::fs::write",
            FsWriteBytes => "std::fs::write_bytes",
            FsExists => "std::fs::exists",
            FsCreateDirAll => "std::fs::create_dir_all",
            FsRemoveFile => "std::fs::remove_file",
            NetTcpConnect => "TcpStream::connect",
            NetTcpRead => "TcpStream::read",
            NetTcpReadToString => "TcpStream::read_to_string",
            NetTcpWrite => "TcpStream::write",
            NetTcpWriteStr => "TcpStream::write_str",
            NetTcpFlush => "TcpStream::flush",
            NetTcpShutdown => "TcpStream::shutdown",
            NetTcpClose => "TcpStream::close",
            NetListenerBind => "TcpListener::bind",
            NetListenerLocalAddr => "TcpListener::local_addr",
            NetListenerAccept => "TcpListener::accept",
            NetListenerClose => "TcpListener::close",
        }
    }
}

impl Builtin {
    /// Whether it may be used in a reversible context.
    pub fn reversible(self) -> bool {
        match self {
            Builtin::Std(op) => op.reversible(),
            _ => !matches!(
                self,
                Builtin::Measure
                    | Builtin::MeasureIn
                    | Builtin::Reset
                    | Builtin::Discard
                    | Builtin::Release
                    | Builtin::Amplitudes
                    | Builtin::Print { .. }
                    | Builtin::Qasm
            ),
        }
    }

    /// Whether calling it needs `unsafe`.
    pub fn is_unsafe(self) -> bool {
        matches!(self, Builtin::Release | Builtin::Qasm)
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
            Builtin::Amplitudes => "amplitudes",
            Builtin::Print { stderr: false } => "print",
            Builtin::Print { stderr: true } => "eprint",
            Builtin::Qasm => "qasm",
            Builtin::Std(op) => op.name(),
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
    /// `unitary(m)`: a gate on a `Qubit` or `[Qubit; N]` given as a `2^N × 2^N` matrix of
    /// `Complex` values. Its entries travel as the gate's parameters: real and imaginary parts,
    /// rows first.
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
    /// `fft(x)`: the discrete Fourier transform of an array of complex numbers.
    Fft,
    /// `ifft(x)`: its inverse.
    Ifft,
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
    pub(crate) fn new(
        receiver: Option<OwnKind>,
        params: Vec<Ty>,
        ret: Ty,
        reversible: Reversible,
    ) -> Self {
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
        // An N-qubit gate from its 2^N × 2^N matrix (`spec/core.md` §1).
        TyKind::Array(Ty::QUBIT, n) if name == "unitary" && (1..=10).contains(&n) => {
            let dim = 1u64 << n;
            let params = vec![matrix(types, dim)];
            Some((
                Builtin::Gate(Gate::Unitary),
                Sig::new(Some(OwnKind::Mut), params, self_mut, Yes),
            ))
        }
        TyKind::Array(Ty::QUBIT, _) | TyKind::SizedArray(Ty::QUBIT, _) if name == "measure" => {
            let bits = types.measured(base)?;
            Some((
                Builtin::Measure,
                Sig::new(Some(OwnKind::Mv), vec![], bits, No),
            ))
        }
        TyKind::Array(Ty::QUBIT, _) | TyKind::SizedArray(Ty::QUBIT, _) if name == "measure_in" => {
            let bits = types.measured(base)?;
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
