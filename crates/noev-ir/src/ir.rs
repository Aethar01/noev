//! The quantum IR: each function is a control-flow graph of basic blocks.
//!
//! Statements assign classical values and apply quantum operations. Blocks end in a
//! [`Terminator`]: a jump, branch, call, assertion or return. `ctrl` and `adj` are kept as
//! operations (control push/pop, an adjoint flag on gates and calls) so that each backend can
//! expand them in its own way.

use std::fmt;

use noev_sema::prelude::{FloatConst, Gate, Math, Pauli};
use noev_sema::{Ty, TyKind, Types};
use noev_syntax::Span;
pub use noev_syntax::ast::{Align, FormatKind, FormatSpec};

macro_rules! index {
    ($($(#[$doc:meta])* $name:ident;)*) => {$(
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u32);

        impl $name {
            /// The position in its array.
            pub fn index(self) -> usize {
                self.0 as usize
            }
        }
    )*};
}

index! {
    /// A function in [`Program::functions`].
    FuncId;
    /// A local in [`Function::locals`]. `Local(0)` holds the return value.
    Local;
    /// A basic block in [`Function::blocks`].
    Block;
}

/// A lowered program.
#[derive(Clone, Debug)]
pub struct Program {
    /// Every function, indexed by [`FuncId`]. Same order as the syntax tree's functions.
    pub functions: Vec<Function>,
    /// `main`, if the program has one.
    pub main: Option<FuncId>,
    /// The program's types.
    pub types: Types,
    /// Struct names, indexed like [`Types::structs`].
    pub struct_names: Vec<String>,
    /// Each struct's field names, in declaration order.
    pub struct_fields: Vec<Vec<String>>,
}

/// A function's control-flow graph.
#[derive(Clone, Debug)]
pub struct Function {
    /// Its name; methods are `Struct::name`.
    pub name: String,
    /// How many parameters it has (including `self`). They are locals `1..=params`.
    pub params: usize,
    /// Every local: the return value, parameters, variables and temporaries.
    pub locals: Vec<LocalDecl>,
    /// The basic blocks. Execution starts at `Block(0)`.
    pub blocks: Vec<BasicBlock>,
    /// Where the function is declared.
    pub span: Span,
}

/// A local's type, and its source name if it has one.
#[derive(Clone, Debug)]
pub struct LocalDecl {
    /// Its type.
    pub ty: Ty,
    /// The variable it came from, if any.
    pub name: Option<String>,
}

/// Straight-line statements followed by one terminator.
#[derive(Clone, Debug)]
pub struct BasicBlock {
    /// The statements, in order.
    pub stmts: Vec<Stmt>,
    /// Where control goes next.
    pub term: Terminator,
}

/// A location: a local followed by projections.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    /// The local.
    pub local: Local,
    /// Field, element and dereference steps.
    pub proj: Vec<Proj>,
}

impl Place {
    /// The local itself.
    pub fn local(local: Local) -> Self {
        Place {
            local,
            proj: Vec::new(),
        }
    }

    /// This place followed by `proj`.
    pub fn with(&self, proj: Proj) -> Self {
        let mut place = self.clone();
        place.proj.push(proj);
        place
    }
}

/// One step of a [`Place`].
#[derive(Clone, Debug, PartialEq)]
pub enum Proj {
    /// Follow a reference.
    Deref,
    /// A struct field or tuple element, by position.
    Field(u32),
    /// An array element.
    Index(Operand),
}

/// A value used by a statement.
#[derive(Clone, Debug, PartialEq)]
pub enum Operand {
    /// The value at a place, which stays usable (copyable types and references).
    Copy(Place),
    /// The value at a place, which is used up (values containing qubits).
    Move(Place),
    /// A constant.
    Const(Const),
}

/// A constant value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Const {
    /// `()`.
    Unit,
    /// An `Int`.
    Int(i64),
    /// A `Float`.
    Float(f64),
    /// A `Bool`.
    Bool(bool),
    /// A `Bit`.
    Bit(bool),
}

/// A unary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    /// `-x` on `Int`, `Float` or `Observable`.
    Neg,
    /// `~x` on `Int` or `Bit`.
    BitNot,
    /// `not x` on `Bool`.
    Not,
}

/// A binary operator. The operand types say which operation it is (`+` on `Int`, `Float` or
/// `Observable`).
pub use noev_syntax::ast::BinaryOp as BinOp;

/// How a value is computed.
#[derive(Clone, Debug, PartialEq)]
pub enum Rvalue {
    /// An operand as it is.
    Use(Operand),
    /// A unary operation.
    Unary(UnOp, Operand),
    /// A binary operation.
    Binary(BinOp, Operand, Operand),
    /// A tuple, array or struct (fields in declaration order), by the destination's type.
    Aggregate(Vec<Operand>),
    /// `[value; count]` of a copyable value.
    Repeat(Operand, u64),
    /// `ref place` or `mut place`.
    Ref {
        /// Whether it is `mut`.
        mutable: bool,
        /// What is referenced.
        place: Place,
    },
    /// `Float(x)` and the other conversions, to the given type.
    Convert(Ty, Operand),
    /// A maths method; the receiver is the first operand.
    Math(Math, Vec<Operand>),
    /// A `Float` constant.
    FloatConst(FloatConst),
    /// `X(i)`, `Y(i)` or `Z(i)`.
    Pauli(Pauli, Operand),
    /// `I()`.
    Identity,
    /// A string literal: text and formatted values.
    Format(Vec<FormatPart>),
    /// `P(i, b)` (`x_basis: false`) or `PX(i, b)` (`x_basis: true`).
    Projector {
        /// Whether it is in the X basis.
        x_basis: bool,
        /// The qubit index.
        index: Operand,
        /// The basis state.
        bit: Operand,
    },
}

/// A piece of a string being built.
#[derive(Clone, Debug, PartialEq)]
pub enum FormatPart {
    /// Literal text.
    Text(String),
    /// A value, formatted according to its type and spec.
    Value(Operand, Ty, Option<FormatSpec>),
}

/// A statement.
#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    /// Write values, separated by spaces and followed by a line break, to standard output or
    /// (`stderr`) standard error.
    Print {
        /// Whether to write to standard error.
        stderr: bool,
        /// The values and their types.
        args: Vec<(Operand, Ty)>,
    },
    /// `place = value`.
    Assign(Place, Rvalue),
    /// Allocate a fresh qubit in \|0⟩ at a `Qubit` place.
    Alloc(Place),
    /// Apply a gate.
    Gate {
        /// Which gate.
        gate: Gate,
        /// Whether to apply its inverse (`adj`).
        adjoint: bool,
        /// The target qubits (two for `swap`).
        qubits: Vec<Place>,
        /// Angles.
        params: Vec<Operand>,
    },
    /// Measure a qubit in the Z basis into a `Bit` place, consuming it.
    Measure {
        /// Where the result goes.
        dest: Place,
        /// The qubit.
        qubit: Place,
    },
    /// Return a qubit to \|0⟩.
    Reset(Place),
    /// Release a qubit, which must be in \|0⟩.
    Release(Place),
    /// Add every qubit in these places as controls of the following gates and calls.
    CtrlPush(Vec<Place>),
    /// Remove the controls added by the matching [`Stmt::CtrlPush`].
    CtrlPop,
    /// `dest = <prep| observable |prep>`.
    Expectation {
        /// Where the `Float` goes.
        dest: Place,
        /// The preparation, run as many times as needed.
        prep: Prep,
        /// The observable.
        observable: Operand,
    },
}

/// The preparation of an expectation value.
#[derive(Clone, Debug, PartialEq)]
pub enum Prep {
    /// A ket literal's state, such as `"+0"`.
    Ket(String),
    /// A call to a function returning qubits, with classical arguments.
    Call(FuncId, Vec<Operand>),
}

/// How a basic block ends.
#[derive(Clone, Debug, PartialEq)]
pub enum Terminator {
    /// Continue at another block.
    Goto(Block),
    /// Continue at `then` if `cond` is true (or a `Bit` is 1), otherwise at `otherwise`.
    Branch {
        /// A `Bool` or `Bit`.
        cond: Operand,
        /// Where to go when true.
        then: Block,
        /// Where to go when false.
        otherwise: Block,
    },
    /// Call a function and continue at `next`.
    Call {
        /// The function.
        func: FuncId,
        /// The arguments.
        args: Vec<Operand>,
        /// Where the result goes.
        dest: Place,
        /// Whether to run it backwards (`adj`).
        adjoint: bool,
        /// Where to continue.
        next: Block,
    },
    /// Stop the program if `cond` is false; otherwise continue at `next`.
    Assert {
        /// A `Bool`.
        cond: Operand,
        /// Where to continue.
        next: Block,
        /// The `assert` statement, for the error message.
        span: Span,
    },
    /// Return `Local(0)` to the caller.
    Return,
    /// Never reached.
    Unreachable,
}

// Printing.

impl Program {
    /// How a type is written.
    pub fn type_name(&self, ty: Ty) -> String {
        match self.types.kind(ty) {
            TyKind::Error => "{error}".into(),
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
                let elems: Vec<String> = elems.iter().map(|&t| self.type_name(t)).collect();
                format!("({})", elems.join(", "))
            }
            TyKind::Array(elem, len) => format!("[{}; {len}]", self.type_name(*elem)),
            TyKind::Struct(id) => self.struct_names[id.index()].clone(),
            TyKind::Ref { mutable, inner } => {
                format!(
                    "{} {}",
                    if *mutable { "mut" } else { "ref" },
                    self.type_name(*inner)
                )
            }
        }
    }
}

impl fmt::Display for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, function) in self.functions.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            self.write_function(f, function)?;
        }
        Ok(())
    }
}

impl Program {
    fn write_function(&self, f: &mut fmt::Formatter<'_>, function: &Function) -> fmt::Result {
        let params: Vec<String> = (1..=function.params)
            .map(|i| format!("_{i}: {}", self.type_name(function.locals[i].ty)))
            .collect();
        writeln!(
            f,
            "fn {}({}) -> {} {{",
            function.name,
            params.join(", "),
            self.type_name(function.locals[0].ty)
        )?;
        for (i, local) in function.locals.iter().enumerate().skip(function.params + 1) {
            let name = local
                .name
                .as_deref()
                .map(|n| format!("  // {n}"))
                .unwrap_or_default();
            writeln!(f, "    let _{i}: {}{name}", self.type_name(local.ty))?;
        }
        for (i, block) in function.blocks.iter().enumerate() {
            writeln!(f, "  bb{i}:")?;
            for stmt in &block.stmts {
                writeln!(f, "    {}", self.stmt_text(stmt))?;
            }
            writeln!(f, "    {}", self.term_text(&block.term))?;
        }
        writeln!(f, "}}")
    }

    fn function_name(&self, func: FuncId) -> &str {
        &self.functions[func.index()].name
    }

    fn stmt_text(&self, stmt: &Stmt) -> String {
        match stmt {
            Stmt::Assign(place, value) => {
                format!("{} = {}", place_text(place), self.rvalue_text(value))
            }
            Stmt::Alloc(place) => format!("{} = alloc", place_text(place)),
            Stmt::Gate {
                gate,
                adjoint,
                qubits,
                params,
            } => {
                let mut text =
                    format!("{}{gate:?}", if *adjoint { "adj " } else { "" }).to_lowercase();
                let args: Vec<String> = qubits
                    .iter()
                    .map(place_text)
                    .chain(params.iter().map(operand_text))
                    .collect();
                text.push_str(&format!(" {}", args.join(", ")));
                text
            }
            Stmt::Measure { dest, qubit } => {
                format!("{} = measure {}", place_text(dest), place_text(qubit))
            }
            Stmt::Reset(place) => format!("reset {}", place_text(place)),
            Stmt::Release(place) => format!("release {}", place_text(place)),
            Stmt::CtrlPush(places) => {
                let places: Vec<String> = places.iter().map(place_text).collect();
                format!("ctrl push {}", places.join(", "))
            }
            Stmt::CtrlPop => "ctrl pop".into(),
            Stmt::Print { stderr, args } => {
                let args: Vec<String> = args.iter().map(|(op, _)| operand_text(op)).collect();
                format!(
                    "{} {}",
                    if *stderr { "eprint" } else { "print" },
                    args.join(", ")
                )
            }
            Stmt::Expectation {
                dest,
                prep,
                observable,
            } => {
                let prep = match prep {
                    Prep::Ket(state) => format!("|{state}>"),
                    Prep::Call(func, args) => {
                        let args: Vec<String> = args.iter().map(operand_text).collect();
                        format!("{}({})", self.function_name(*func), args.join(", "))
                    }
                };
                format!(
                    "{} = expectation {prep}, {}",
                    place_text(dest),
                    operand_text(observable)
                )
            }
        }
    }

    fn rvalue_text(&self, value: &Rvalue) -> String {
        let list = |ops: &[Operand]| ops.iter().map(operand_text).collect::<Vec<_>>().join(", ");
        match value {
            Rvalue::Use(op) => operand_text(op),
            Rvalue::Unary(op, x) => {
                let op = match op {
                    UnOp::Neg => "-",
                    UnOp::BitNot => "~",
                    UnOp::Not => "not ",
                };
                format!("{op}{}", operand_text(x))
            }
            Rvalue::Binary(op, a, b) => {
                format!("{} {} {}", operand_text(a), op.as_str(), operand_text(b))
            }
            Rvalue::Aggregate(ops) => format!("{{{}}}", list(ops)),
            Rvalue::Repeat(op, n) => format!("[{}; {n}]", operand_text(op)),
            Rvalue::Ref { mutable, place } => {
                format!(
                    "{} {}",
                    if *mutable { "mut" } else { "ref" },
                    place_text(place)
                )
            }
            Rvalue::Convert(ty, op) => format!("{}({})", self.type_name(*ty), operand_text(op)),
            Rvalue::Math(math, ops) => format!("{math:?}({})", list(ops)).to_lowercase(),
            Rvalue::FloatConst(c) => format!(
                "Float::{}",
                match c {
                    FloatConst::Pi => "PI",
                    FloatConst::Tau => "TAU",
                    FloatConst::E => "E",
                }
            ),
            Rvalue::Pauli(p, i) => format!("{p:?}({})", operand_text(i)),
            Rvalue::Identity => "I()".into(),
            Rvalue::Format(parts) => {
                let text: String = parts
                    .iter()
                    .map(|part| match part {
                        FormatPart::Text(t) => t.escape_debug().to_string(),
                        FormatPart::Value(op, _, _) => format!("{{{}}}", operand_text(op)),
                    })
                    .collect();
                format!("format \"{text}\"")
            }
            Rvalue::Projector {
                x_basis,
                index,
                bit,
            } => {
                format!(
                    "{}({}, {})",
                    if *x_basis { "PX" } else { "P" },
                    operand_text(index),
                    operand_text(bit)
                )
            }
        }
    }

    fn term_text(&self, term: &Terminator) -> String {
        match term {
            Terminator::Goto(b) => format!("goto bb{}", b.0),
            Terminator::Branch {
                cond,
                then,
                otherwise,
            } => {
                format!(
                    "branch {} ? bb{} : bb{}",
                    operand_text(cond),
                    then.0,
                    otherwise.0
                )
            }
            Terminator::Call {
                func,
                args,
                dest,
                adjoint,
                next,
            } => {
                let args: Vec<String> = args.iter().map(operand_text).collect();
                format!(
                    "{} = {}{}({}) -> bb{}",
                    place_text(dest),
                    if *adjoint { "adj " } else { "" },
                    self.function_name(*func),
                    args.join(", "),
                    next.0
                )
            }
            Terminator::Assert { cond, next, .. } => {
                format!("assert {} -> bb{}", operand_text(cond), next.0)
            }
            Terminator::Return => "return".into(),
            Terminator::Unreachable => "unreachable".into(),
        }
    }
}

fn place_text(place: &Place) -> String {
    let mut text = format!("_{}", place.local.0);
    for proj in &place.proj {
        match proj {
            Proj::Deref => text = format!("(*{text})"),
            Proj::Field(i) => text.push_str(&format!(".{i}")),
            Proj::Index(op) => text.push_str(&format!("[{}]", operand_text(op))),
        }
    }
    text
}

fn operand_text(op: &Operand) -> String {
    match op {
        Operand::Copy(place) => place_text(place),
        Operand::Move(place) => format!("move {}", place_text(place)),
        Operand::Const(c) => match c {
            Const::Unit => "()".into(),
            Const::Int(v) => v.to_string(),
            Const::Float(v) => format!("{v:?}"),
            Const::Bool(v) => v.to_string(),
            Const::Bit(v) => format!("{}b", u8::from(*v)),
        },
    }
}
