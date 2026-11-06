//! The interface between the runtime and quantum hardware (`spec/runtime.md` §3).
//!
//! A [`Backend`] accepts work at circuit level (a whole [`Circuit`] and a number of shots), at
//! gate level (a [`Session`] taking one operation at a time), or both. The runtime chooses
//! between them for each run, as `spec/runtime.md` §3 says.

pub mod ketrs;

use noev_sema::prelude::Gate;

/// A failure reported by a backend.
#[derive(Clone, Debug, PartialEq)]
pub struct BackendError(pub String);

/// A gate with its qubits resolved.
#[derive(Clone, Debug, PartialEq)]
pub struct Op {
    /// The gate.
    pub gate: Gate,
    /// Whether it is inverted (`adj`).
    pub adjoint: bool,
    /// Its angles.
    pub params: Vec<f64>,
    /// The qubits controlling it, from enclosing `ctrl` blocks.
    pub controls: Vec<usize>,
    /// The qubits it acts on.
    pub targets: Vec<usize>,
}

/// One step of a [`Circuit`].
#[derive(Clone, Debug, PartialEq)]
pub enum Instruction {
    /// A gate.
    Gate(Op),
    /// Measure a qubit into a classical bit.
    Measure {
        /// The qubit.
        qubit: usize,
        /// The classical bit.
        clbit: usize,
    },
    /// Return a qubit to \|0⟩.
    Reset(usize),
}

/// A run recorded as a circuit: what goes to a circuit-level backend.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Circuit {
    /// How many qubits it uses, all starting in \|0⟩.
    pub qubits: usize,
    /// How many classical bits: one per measurement.
    pub clbits: usize,
    /// The steps, in order.
    pub instructions: Vec<Instruction>,
}

/// The counts of a circuit's outcomes: `(bits, count)`, classical bit `k` at bit `k`.
pub type Counts = Vec<(u64, usize)>;

/// Something that runs quantum operations: a simulator, or a quantum computer.
pub trait Backend {
    /// Its name, for messages.
    fn name(&self) -> &str;

    /// A new gate-level session for one run, starting with no qubits, or `None` if the
    /// backend only accepts circuits. The same seed gives the same results.
    fn session(&self, seed: u64) -> Option<Box<dyn Session>>;

    /// Runs `circuit` `shots` times, or `None` if the backend does not accept this circuit
    /// (it only works gate by gate, or the circuit is too large for it as a circuit).
    fn run_circuit(
        &self,
        circuit: &Circuit,
        shots: usize,
        seed: u64,
    ) -> Option<Result<Counts, BackendError>>;
}

/// One run on a gate-level backend. Operations arrive as the program reaches them. Nothing is
/// measured unless the program measures; when it does, the result comes back before
/// anything else is sent.
pub trait Session {
    /// Adds a qubit in \|0⟩ and returns its index; indices count up from 0.
    fn add_qubit(&mut self) -> Result<usize, BackendError>;

    /// Applies a gate.
    fn apply(&mut self, op: &Op) -> Result<(), BackendError>;

    /// Measures a qubit in the Z basis, leaving it in the state it was measured in.
    fn measure(&mut self, qubit: usize) -> Result<bool, BackendError>;

    /// Returns a qubit to \|0⟩.
    fn reset(&mut self, qubit: usize) -> Result<(), BackendError>;

    /// Called when a qubit the program claims is in \|0⟩ is released. A simulator checks the
    /// claim (`spec/ownership.md` §11); hardware cannot.
    fn release(&mut self, _qubit: usize) -> Result<(), BackendError> {
        Ok(())
    }

    // What only a simulator can do. Each returns `None` on a backend that cannot.

    /// The exact expectation value of a sum of Pauli products, each `(coefficient, [(qubit,
    /// Pauli)])`.
    fn expectation(
        &self,
        _terms: &[(f64, Vec<(usize, Pauli)>)],
    ) -> Option<Result<f64, BackendError>> {
        None
    }

    /// Samples the current state `shots` times without changing it, returning how often each
    /// outcome of `qubits` occurred, `qubits[k]` at bit `k`.
    fn sample(&mut self, _qubits: &[usize], _shots: usize) -> Option<Counts> {
        None
    }
}

/// A Pauli operator, for expectation values.
pub use crate::value::Pauli;

/// A backend that keeps a copy of every circuit job it is given, with its shots, then passes
/// the job on to `inner`. This is how `noev run --circuits` saves them.
pub struct Recorder<'b> {
    /// The backend that runs everything.
    pub inner: &'b dyn Backend,
    /// The circuits run so far, in order, and how many shots each had.
    pub jobs: std::cell::RefCell<Vec<(Circuit, usize)>>,
}

impl<'b> Recorder<'b> {
    /// A recorder in front of `inner`.
    pub fn new(inner: &'b dyn Backend) -> Self {
        Recorder {
            inner,
            jobs: Default::default(),
        }
    }
}

impl Backend for Recorder<'_> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn session(&self, seed: u64) -> Option<Box<dyn Session>> {
        self.inner.session(seed)
    }

    fn run_circuit(
        &self,
        circuit: &Circuit,
        shots: usize,
        seed: u64,
    ) -> Option<Result<Counts, BackendError>> {
        let result = self.inner.run_circuit(circuit, shots, seed);
        if result.is_some() {
            self.jobs.borrow_mut().push((circuit.clone(), shots));
        }
        result
    }
}
