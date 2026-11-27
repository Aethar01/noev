//! The Noev Runtime C ABI (`spec/runtime.md` §4).
//!
//! Shaped like QIR: an opaque [`NoevRuntime`] handle, pointer-sized [`NoevQubit`] handles,
//! tagged [`NoevResult`] handles, and one `extern "C"` function per core gate and runtime
//! operation. Both compiled code (Cranelift, LLVM) and the interpreter ([`crate::Machine`])
//! drive quantum backends through this interface.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::io::Write;
use std::ptr;
use std::rc::Rc;

use noev_sema::prelude::Gate;
use noev_syntax::Span;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use crate::backend::{
    Backend, BackendError, Circuit, Condition, Exporter, Instruction, Op, Session,
};
use crate::value::{Distribution, Observable, Pauli, Value};

/// Where a program's output goes.
pub struct Output {
    /// Standard output.
    pub stdout: Box<dyn Write>,
    /// Standard error.
    pub stderr: Box<dyn Write>,
}

/// A failure while running a program.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeError {
    /// What went wrong.
    pub message: String,
    /// Where, when known.
    pub span: Option<Span>,
    /// Whether, rather than failing, the program needed a result that a backend which only
    /// records circuits does not have (`spec/runtime.md` §7).
    pub unknown: bool,
}

impl RuntimeError {
    /// A failure.
    pub fn new(message: impl Into<String>) -> Self {
        RuntimeError {
            message: message.into(),
            span: None,
            unknown: false,
        }
    }

    /// The error, placed at `span` if it has no place yet.
    pub fn at(mut self, span: Span) -> Self {
        if self.span.is_none() && span != Span::default() {
            self.span = Some(span);
        }
        self
    }

    /// The program needs a result that is not known because circuits are only recorded.
    pub fn unknown(message: impl Into<String>) -> Self {
        RuntimeError {
            unknown: true,
            ..RuntimeError::new(message)
        }
    }
}

/// How the runtime treats quantum operations.
#[derive(Clone, Debug, Default)]
pub enum Mode {
    /// Send them to the backend, one at a time.
    #[default]
    Simulate,
    /// Record them into a circuit instead, without simulating. Measurement results are the
    /// circuit's classical bits ([`NoevResult`] with bit 63 set); where a known value is
    /// needed, they are taken from the recording's `known` outcome, or are a placeholder `0`.
    Build(Recording),
}

/// A circuit being recorded in [`Mode::Build`].
#[derive(Clone, Debug, Default)]
pub struct Recording {
    /// The circuit so far.
    pub circuit: Circuit,
    /// Released and measured qubits, and whether each needs a reset before reuse.
    pub(crate) free: Vec<(usize, bool)>,
    /// One outcome of the circuit, classical bit `k` at bit `k`, when re-running its classical
    /// code for that outcome. Recording the same way again keeps the bits numbered the same.
    pub known: Option<u64>,
    /// How many recorded loops enclose the current point: a qubit allocated in a loop's body
    /// may have been used in the previous pass, so it is reset.
    pub(crate) loops: u32,
    /// The conditions the enclosing recorded branches decided, and how: inside
    /// `if (c[0] == 0)`, testing `c[0] == 0` again needs no `if` of its own.
    pub(crate) decided: Vec<(Condition, bool)>,
    /// When recording `main` as a one-shot circuit: the results of the repeated runs it
    /// starts (`sample`, expectation values), in order. They are computed while recording and
    /// reused when its classical code runs for the outcome, so each runs once.
    pub runs: Option<VecDeque<Value>>,
}

impl Recording {
    /// A recording that re-runs a circuit's classical code for one outcome.
    pub fn knowing(bits: u64) -> Self {
        Recording {
            known: Some(bits),
            ..Recording::default()
        }
    }
}

/// Recorded circuits use fresh qubits up to this many, so their measurements can all come at
/// the end; beyond it they reuse measured qubits after a reset.
const FRESH_QUBITS: usize = 24;

/// High bit set on [`NoevResult`] when the handle names a recorded circuit classical bit
/// rather than a concrete `0`/`1` measurement outcome.
const RESULT_CLBIT_TAG: u64 = 1u64 << 63;

/// Opaque pointer-sized qubit handle across the C ABI (`spec/runtime.md` §4).
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct NoevQubit(pub usize);

/// Tagged measurement result handle across the C ABI (`spec/runtime.md` §4).
///
/// - Bit 63 clear: concrete outcome (`0` or `1`).
/// - Bit 63 set: recorded circuit classical bit index (`value & !(1 << 63)`).
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct NoevResult(pub u64);

impl NoevResult {
    /// A concrete measurement outcome (`0` or `1`).
    pub const fn bit(value: bool) -> Self {
        NoevResult(value as u64)
    }

    /// A recorded circuit classical bit handle.
    pub const fn clbit(index: usize) -> Self {
        NoevResult(RESULT_CLBIT_TAG | (index as u64))
    }

    /// Whether this handle stands for a recorded circuit classical bit.
    pub const fn is_clbit(self) -> bool {
        (self.0 & RESULT_CLBIT_TAG) != 0
    }

    /// The classical bit index, when [`Self::is_clbit`] is true.
    pub const fn clbit_index(self) -> usize {
        (self.0 & !RESULT_CLBIT_TAG) as usize
    }

    /// The concrete bit value, when [`Self::is_clbit`] is false.
    pub const fn as_bit(self) -> bool {
        self.0 != 0
    }
}

/// A complex number across the C ABI.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NoevComplex {
    /// Real part.
    pub re: f64,
    /// Imaginary part.
    pub im: f64,
}

/// One `(qubit, Pauli)` factor in a Pauli product (`0` = X, `1` = Y, `2` = Z).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoevPauliFactor {
    /// Target qubit.
    pub qubit: NoevQubit,
    /// `0` = X, `1` = Y, `2` = Z.
    pub pauli: u8,
}

/// One weighted Pauli product term in an observable.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct NoevPauliTerm {
    /// Real coefficient.
    pub coeff: f64,
    /// Pointer to `len` factors (may be null when `len == 0`, the identity term).
    pub factors: *const NoevPauliFactor,
    /// Number of factors.
    pub len: usize,
}

/// Shot body callback passed to [`noev_rt_sample_run`].
pub type NoevRunFn = extern "C" fn(rt: *mut NoevRuntime, env: *mut c_void) -> bool;

/// Per-outcome callback passed to [`noev_rt_sample_run`], called once per distinct outcome
/// with the shot/replay runtime and how many shots produced that outcome.
pub type NoevShotCallback =
    extern "C" fn(rt: *mut NoevRuntime, count: usize, env: *mut c_void) -> bool;

/// Recorded operations, ancillas and saved outer controls for one `with`/`do` block.
struct WithFrame {
    ops: Vec<Op>,
    allocs: Vec<usize>,
    controls: Vec<usize>,
    control_marks: Vec<usize>,
}

/// Either an owned backend (created via C ABI constructors) or a borrowed backend whose
/// lifetime is managed by an enclosing Rust owner (such as [`crate::Machine`]).
enum BackendRef {
    Owned(Box<dyn Backend>),
    Borrowed(*const (dyn Backend + 'static)),
}

impl BackendRef {
    fn as_ptr(&self) -> *const (dyn Backend + 'static) {
        match self {
            BackendRef::Owned(b) => b.as_ref() as *const dyn Backend,
            BackendRef::Borrowed(ptr) => *ptr,
        }
    }

    fn get(&self) -> &dyn Backend {
        match self {
            BackendRef::Owned(b) => b.as_ref(),
            BackendRef::Borrowed(ptr) => unsafe { &**ptr },
        }
    }
}

/// Opaque runtime execution context (`spec/runtime.md` §4).
///
/// Owns the gate-level backend session (or circuit recording state), free qubit pool, `ctrl`
/// stack, `adj` and `with`/`do` recording frames, PRNG, output streams, and last error.
pub struct NoevRuntime {
    backend: BackendRef,
    session: Option<Box<dyn Session>>,
    free: Vec<usize>,
    controls: Vec<usize>,
    control_marks: Vec<usize>,
    recording: Vec<Vec<Op>>,
    adj_releases: Vec<Vec<usize>>,
    with_active: Vec<WithFrame>,
    with_pending: Vec<WithFrame>,
    rng: SmallRng,
    measured: bool,
    output: Option<Rc<RefCell<Output>>>,
    /// Simulating, or recording a circuit (possibly for one known outcome).
    pub mode: Mode,
    /// Whether, while recording, an operation needed a measurement result's actual value.
    pub needs_values: Cell<bool>,
    /// Error from the most recent failing C ABI call, if any.
    last_error: Option<RuntimeError>,
    /// Heap arenas for compiled native code (arbitrary-precision ints, strings, observables,
    /// distributions). Freed automatically when the runtime is dropped.
    pub(crate) bigints: Vec<num_bigint::BigInt>,
    pub(crate) strings: Vec<String>,
    pub(crate) observables: Vec<Observable>,
    pub(crate) distributions: Vec<Rc<Distribution>>,
}

impl NoevRuntime {
    /// Creates a runtime owning `backend`, seeded with `seed`.
    pub fn new_owned(backend: Box<dyn Backend>, seed: u64) -> Self {
        let mut rng = SmallRng::seed_from_u64(seed);
        let session = backend.session(rng.random());
        Self::from_parts(BackendRef::Owned(backend), session, rng)
    }

    /// Creates a runtime borrowing `backend`.
    ///
    /// # Safety
    /// The caller must ensure `backend` outlives the returned [`NoevRuntime`].
    pub unsafe fn new_borrowed<'b>(backend: &'b dyn Backend, seed: u64) -> Self {
        let mut rng = SmallRng::seed_from_u64(seed);
        let session = backend.session(rng.random());
        // SAFETY: Erasing the trait object lifetime to `'static` is sound because the caller
        // guarantees `backend` outlives this `NoevRuntime`.
        let ptr: *const (dyn Backend + 'static) =
            unsafe { std::mem::transmute::<*const (dyn Backend + 'b), _>(backend) };
        Self::from_parts(BackendRef::Borrowed(ptr), session, rng)
    }

    unsafe fn new_from_ptr(backend: *const (dyn Backend + 'static), seed: u64) -> Self {
        let mut rng = SmallRng::seed_from_u64(seed);
        let session = unsafe { &*backend }.session(rng.random());
        Self::from_parts(BackendRef::Borrowed(backend), session, rng)
    }

    fn from_parts(backend: BackendRef, session: Option<Box<dyn Session>>, rng: SmallRng) -> Self {
        NoevRuntime {
            backend,
            session,
            free: Vec::new(),
            controls: Vec::new(),
            control_marks: Vec::new(),
            recording: Vec::new(),
            adj_releases: Vec::new(),
            with_active: Vec::new(),
            with_pending: Vec::new(),
            rng,
            measured: false,
            output: None,
            mode: Mode::Simulate,
            needs_values: Cell::new(false),
            last_error: None,
            bigints: Vec::new(),
            strings: vec![String::new()],
            observables: vec![Observable::default()],
            distributions: Vec::new(),
        }
    }

    /// Creates a child runtime sharing the same backend pointer, output, and value arenas, with
    /// a fresh seed.
    pub(crate) unsafe fn child_runtime(&self, seed: u64) -> Self {
        let mut child = unsafe { Self::new_from_ptr(self.backend.as_ptr(), seed) };
        child.output = self.output.clone();
        child.bigints = self.bigints.clone();
        child.strings = self.strings.clone();
        child.observables = self.observables.clone();
        child.distributions = self.distributions.clone();
        child
    }

    /// Stores `err` as the last error and returns `false`.
    pub(crate) fn fail_err(&mut self, error: RuntimeError) -> bool {
        self.fail(error)
    }

    /// Applies an [`Op`] through the runtime's recording/execution pipeline.
    pub(crate) fn emit_gate_op(
        &mut self,
        gate: Gate,
        adjoint: bool,
        params: Vec<f64>,
        targets: Vec<usize>,
    ) -> Result<(), RuntimeError> {
        self.emit_gate(gate, adjoint, params, targets)
    }

    /// Releases a qubit through the runtime.
    pub(crate) fn release_q(&mut self, qubit: usize) -> Result<(), RuntimeError> {
        self.release_inner(qubit)
    }

    /// Allocates a qubit through the runtime.
    pub(crate) fn alloc_q(&mut self) -> Result<usize, RuntimeError> {
        self.allocate_inner()
    }

    /// Measures a qubit through the runtime.
    pub(crate) fn measure_q(&mut self, qubit: usize) -> Result<NoevResult, RuntimeError> {
        self.measure_inner(qubit)
    }

    /// Sets the execution mode (`Mode::Simulate` or `Mode::Build`).
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Routes `print`/`eprint` output to `output`.
    pub fn set_output(&mut self, output: Rc<RefCell<Output>>) {
        self.output = Some(output);
    }

    /// The underlying backend.
    pub fn backend(&self) -> &dyn Backend {
        self.backend.get()
    }

    /// Takes the last error, returning a fallback message if none was stored.
    pub fn take_error(&mut self) -> RuntimeError {
        self.last_error
            .take()
            .unwrap_or_else(|| RuntimeError::new("runtime error"))
    }

    /// Sets `last_error` and returns `false` for C ABI error propagation.
    fn fail(&mut self, error: RuntimeError) -> bool {
        self.last_error = Some(error);
        false
    }

    /// Draws the next deterministic seed from the runtime's PRNG.
    pub fn next_seed(&mut self) -> u64 {
        self.rng.random()
    }

    /// Whether any measurement has happened in this run.
    pub fn measured(&self) -> bool {
        self.measured
    }

    /// Samples the current statevector `shots` times without modifying it (`Session::sample`).
    pub fn sample_state(&mut self, qubits: &[usize], shots: usize) -> Option<Vec<(u64, usize)>> {
        self.session.as_mut()?.sample(qubits, shots)
    }

    /// Starts recording operations into a fresh `adj`/prep buffer without executing them.
    pub(crate) fn record_ops_start(&mut self) {
        self.recording.push(Vec::new());
        self.adj_releases.push(Vec::new());
    }

    /// Stops recording and returns the recorded operations and deferred releases.
    pub(crate) fn record_ops_end(&mut self) -> (Vec<Op>, Vec<usize>) {
        let ops = self.recording.pop().unwrap_or_default();
        let releases = self.adj_releases.pop().unwrap_or_default();
        (ops, releases)
    }

    /// Replays `ops` (recorded on `from`) onto `onto`, inverted when `inverse` and controlled
    /// by `control` if given. Temporary ancillas get fresh qubits released afterwards.
    pub(crate) fn replay_ops(
        &mut self,
        ops: Vec<Op>,
        from: &[usize],
        onto: &[usize],
        inverse: bool,
        control: Option<usize>,
    ) -> Result<(), RuntimeError> {
        if from.len() != onto.len() {
            return Err(RuntimeError::new(format!(
                "the preparation makes {} qubits, but there are {}",
                from.len(),
                onto.len()
            )));
        }
        let mut temps: Vec<(usize, usize)> = Vec::new();
        let ops: Vec<Op> = if inverse {
            ops.into_iter().rev().collect()
        } else {
            ops
        };
        let result = (|| -> Result<(), RuntimeError> {
            for mut op in ops {
                op.adjoint ^= inverse;
                let mut map = |q: usize| -> Result<usize, RuntimeError> {
                    if let Some(k) = from.iter().position(|&f| f == q) {
                        return Ok(onto[k]);
                    }
                    if let Some(&(_, mapped)) = temps.iter().find(|&&(src, _)| src == q) {
                        return Ok(mapped);
                    }
                    let fresh = self.allocate_inner()?;
                    temps.push((q, fresh));
                    Ok(fresh)
                };
                let mut targets = Vec::with_capacity(op.targets.len());
                for q in op.targets {
                    targets.push(map(q)?);
                }
                let mut controls =
                    Vec::with_capacity(op.controls.len() + usize::from(control.is_some()));
                for q in op.controls {
                    controls.push(map(q)?);
                }
                controls.extend(control);
                op.targets = targets;
                op.controls = controls;
                self.emit_op(op)?;
            }
            Ok(())
        })();
        for (_, fresh) in temps.into_iter().rev() {
            self.release_inner(fresh)?;
        }
        result
    }

    fn session_mut(&mut self) -> Result<&mut dyn Session, RuntimeError> {
        let name = self.backend.get().name();
        let executes = self.backend.get().executes();
        match &mut self.session {
            Some(session) => Ok(session.as_mut()),
            None if !executes => Err(RuntimeError::unknown(
                "the program runs a quantum operation outside a `sample`",
            )),
            None => Err(RuntimeError::new(format!(
                "`{name}` only runs circuits, and this run is not one (see `noev build --emit=plan`)"
            ))),
        }
    }

    fn allocate_inner(&mut self) -> Result<usize, RuntimeError> {
        let qubit = match &mut self.mode {
            Mode::Simulate => {
                if let Some(qubit) = self.free.pop() {
                    qubit
                } else {
                    self.session_mut()?.add_qubit().map_err(backend_error)?
                }
            }
            Mode::Build(recording) => {
                let circuit = &mut recording.circuit;
                let clean = (recording.loops == 0)
                    .then(|| recording.free.iter().rposition(|&(_, dirty)| !dirty))
                    .flatten();
                let (qubit, needs_reset) = if let Some(i) = clean {
                    recording.free.remove(i)
                } else if circuit.qubits < FRESH_QUBITS || recording.free.is_empty() {
                    circuit.qubits += 1;
                    (circuit.qubits - 1, false)
                } else {
                    recording.free.pop().expect("checked above")
                };
                if needs_reset || recording.loops > 0 {
                    circuit.instructions.push(Instruction::Reset(qubit));
                }
                qubit
            }
        };
        if let Some(frame) = self.with_active.last_mut() {
            frame.allocs.push(qubit);
        }
        Ok(qubit)
    }

    fn release_inner(&mut self, qubit: usize) -> Result<(), RuntimeError> {
        if let Some(pending) = self.adj_releases.last_mut() {
            if !pending.contains(&qubit) {
                pending.push(qubit);
            }
            match &mut self.mode {
                Mode::Simulate => self.free.push(qubit),
                Mode::Build(recording) => recording.free.push((qubit, false)),
            }
            return Ok(());
        }
        match &mut self.mode {
            Mode::Simulate => {}
            Mode::Build(recording) => {
                recording.free.push((qubit, false));
                return Ok(());
            }
        }
        self.session_mut()?.release(qubit).map_err(backend_error)?;
        self.free.push(qubit);
        Ok(())
    }

    fn verify_release_inner(&mut self, qubit: usize) -> Result<(), RuntimeError> {
        if let Some(outer) = self.adj_releases.last_mut() {
            if !outer.contains(&qubit) {
                outer.push(qubit);
            }
            return Ok(());
        }
        if matches!(self.mode, Mode::Simulate) {
            self.session_mut()?.release(qubit).map_err(backend_error)?;
        }
        Ok(())
    }

    fn reset_inner(&mut self, qubit: usize) -> Result<(), RuntimeError> {
        match &mut self.mode {
            Mode::Simulate => self.session_mut()?.reset(qubit).map_err(backend_error),
            Mode::Build(recording) => {
                recording
                    .circuit
                    .instructions
                    .push(Instruction::Reset(qubit));
                Ok(())
            }
        }
    }

    fn measure_inner(&mut self, qubit: usize) -> Result<NoevResult, RuntimeError> {
        self.measured = true;
        match &mut self.mode {
            Mode::Simulate => {}
            Mode::Build(recording) => {
                let circuit = &mut recording.circuit;
                let clbit = circuit.clbits;
                circuit
                    .instructions
                    .push(Instruction::Measure { qubit, clbit });
                circuit.clbits += 1;
                recording.free.push((qubit, true));
                return Ok(NoevResult::clbit(clbit));
            }
        }
        let session = self.session_mut()?;
        let bit = session.measure(qubit).map_err(backend_error)?;
        if bit {
            session
                .apply(&Op {
                    gate: Gate::X,
                    adjoint: false,
                    params: Vec::new(),
                    controls: Vec::new(),
                    targets: vec![qubit],
                })
                .map_err(backend_error)?;
        }
        self.free.push(qubit);
        Ok(NoevResult::bit(bit))
    }

    fn emit_gate(
        &mut self,
        gate: Gate,
        adjoint: bool,
        params: Vec<f64>,
        targets: Vec<usize>,
    ) -> Result<(), RuntimeError> {
        let op = Op {
            gate,
            adjoint,
            params,
            controls: self.controls.clone(),
            targets,
        };
        self.emit_op(op)
    }

    fn emit_op(&mut self, op: Op) -> Result<(), RuntimeError> {
        for frame in &mut self.with_active {
            frame.ops.push(op.clone());
        }
        match self.recording.last_mut() {
            Some(recording) => {
                recording.push(op);
                Ok(())
            }
            None => self.execute_op(op),
        }
    }

    fn execute_op(&mut self, op: Op) -> Result<(), RuntimeError> {
        if op.gate == Gate::Unitary
            && op.targets.len() == 2
            && (!op.controls.is_empty() || matches!(self.mode, Mode::Build(_)))
        {
            let (first, second) = (op.targets[0], op.targets[1]);
            for mut part in crate::unitary::decompose(&op.params, op.adjoint, first, second) {
                part.controls.extend(&op.controls);
                self.execute_op(part)?;
            }
            return Ok(());
        }
        if op.controls.iter().any(|c| op.targets.contains(c)) {
            return Err(RuntimeError::new(
                "a gate's target is also one of its controls",
            ));
        }
        let wanted = match op.gate {
            Gate::Swap => 2,
            Gate::Unitary => crate::unitary::size(&op.params) / 2,
            _ => 1,
        };
        if op.targets.len() != wanted {
            return Err(RuntimeError::new("a gate with the wrong number of qubits"));
        }
        match &mut self.mode {
            Mode::Simulate => self.session_mut()?.apply(&op).map_err(backend_error),
            Mode::Build(recording) => {
                recording.circuit.instructions.push(Instruction::Gate(op));
                Ok(())
            }
        }
    }

    fn with_end_inner(&mut self) -> Result<(), RuntimeError> {
        let frame = self
            .with_pending
            .pop()
            .ok_or_else(|| RuntimeError::new("`with` end without matching `do`"))?;
        let controls = std::mem::take(&mut self.controls);
        let control_marks = std::mem::take(&mut self.control_marks);
        let result = (|| -> Result<(), RuntimeError> {
            for mut op in frame.ops.into_iter().rev() {
                op.adjoint = !op.adjoint;
                self.emit_op(op)?;
            }
            Ok(())
        })();
        self.controls = controls;
        self.control_marks = control_marks;
        result?;
        for qubit in frame.allocs.into_iter().rev() {
            self.release_inner(qubit)?;
        }
        Ok(())
    }

    fn adj_end_inner(&mut self) -> Result<(), RuntimeError> {
        let ops = self
            .recording
            .pop()
            .ok_or_else(|| RuntimeError::new("`adj` end without matching start"))?;
        let releases = self.adj_releases.pop().unwrap_or_default();
        for mut op in ops.into_iter().rev() {
            op.adjoint = !op.adjoint;
            self.emit_op(op)?;
        }
        for qubit in releases {
            self.verify_release_inner(qubit)?;
        }
        Ok(())
    }
}

fn backend_error(error: BackendError) -> RuntimeError {
    RuntimeError::new(error.0)
}

unsafe fn rt_mut<'a>(rt: *mut NoevRuntime) -> Option<&'a mut NoevRuntime> {
    if rt.is_null() {
        None
    } else {
        Some(unsafe { &mut *rt })
    }
}

unsafe fn rt_ref<'a>(rt: *const NoevRuntime) -> Option<&'a NoevRuntime> {
    if rt.is_null() {
        None
    } else {
        Some(unsafe { &*rt })
    }
}

fn gate_result(rt: *mut NoevRuntime, res: Result<(), RuntimeError>) -> bool {
    match res {
        Ok(()) => true,
        Err(err) => {
            if let Some(rt) = unsafe { rt_mut(rt) } {
                rt.fail(err);
            }
            false
        }
    }
}

// ---------------------------------------------------------------------------------------
// Program lifecycle and errors
// ---------------------------------------------------------------------------------------

/// Creates a runtime backed by the `ketrs` simulator, seeded with `seed`.
/// Free with [`noev_rt_free`].
#[cfg(feature = "ketrs")]
#[unsafe(no_mangle)]
pub extern "C" fn noev_rt_new_ketrs(seed: u64) -> *mut NoevRuntime {
    let mut rt = NoevRuntime::new_owned(Box::new(crate::backend::ketrs::Ketrs), seed);
    rt.set_output(Rc::new(RefCell::new(Output {
        stdout: Box::new(std::io::stdout()),
        stderr: Box::new(std::io::stderr()),
    })));
    Box::into_raw(Box::new(rt))
}

/// Creates a runtime backed by the circuit [`Exporter`], seeded with `seed`.
/// Free with [`noev_rt_free`].
#[unsafe(no_mangle)]
pub extern "C" fn noev_rt_new_export(seed: u64) -> *mut NoevRuntime {
    let mut rt = NoevRuntime::new_owned(Box::new(Exporter::default()), seed);
    rt.set_mode(Mode::Build(Recording::default()));
    Box::into_raw(Box::new(rt))
}

/// Flushes output and frees a [`NoevRuntime`] created by `noev_rt_new_*`.
///
/// # Safety
/// `rt` must be null or a pointer returned by `noev_rt_new_*` that has not been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_free(rt: *mut NoevRuntime) {
    if rt.is_null() {
        return;
    }
    let _ = unsafe { noev_rt_flush(rt) };
    drop(unsafe { Box::from_raw(rt) });
}

/// Whether the last operation on `rt` recorded an error.
///
/// # Safety
/// `rt` must be null or a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_has_error(rt: *const NoevRuntime) -> bool {
    unsafe { rt_ref(rt) }.is_some_and(|rt| rt.last_error.is_some())
}

/// Returns the UTF-8 error message of the last error on `rt` (or null if none), writing its
/// byte length to `out_len` if non-null. Valid until the next call on `rt`.
///
/// # Safety
/// `rt` must be null or valid; `out_len` must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_error_message(
    rt: *const NoevRuntime,
    out_len: *mut usize,
) -> *const u8 {
    let Some(rt) = (unsafe { rt_ref(rt) }) else {
        if !out_len.is_null() {
            unsafe { *out_len = 0 };
        }
        return ptr::null();
    };
    match &rt.last_error {
        Some(err) => {
            if !out_len.is_null() {
                unsafe { *out_len = err.message.len() };
            }
            err.message.as_ptr()
        }
        None => {
            if !out_len.is_null() {
                unsafe { *out_len = 0 };
            }
            ptr::null()
        }
    }
}

/// Whether the last error is an export-mode unknown-result stop (`spec/runtime.md` §7).
///
/// # Safety
/// `rt` must be null or a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_error_is_unknown(rt: *const NoevRuntime) -> bool {
    unsafe { rt_ref(rt) }.is_some_and(|rt| rt.last_error.as_ref().is_some_and(|e| e.unknown))
}

/// Clears any error stored on `rt`.
///
/// # Safety
/// `rt` must be null or a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_clear_error(rt: *mut NoevRuntime) {
    if let Some(rt) = unsafe { rt_mut(rt) } {
        rt.last_error = None;
    }
}

/// Stores a UTF-8 runtime error message on `rt`.
///
/// # Safety
/// `rt` must be null or valid; `msg` must point to `len` readable bytes when `len > 0`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_set_error(rt: *mut NoevRuntime, msg: *const u8, len: usize) {
    let Some(rt) = (unsafe { rt_mut(rt) }) else {
        return;
    };
    let text = if msg.is_null() || len == 0 {
        String::from("runtime error")
    } else {
        let slice = unsafe { std::slice::from_raw_parts(msg, len) };
        String::from_utf8_lossy(slice).into_owned()
    };
    rt.last_error = Some(RuntimeError::new(text));
}

/// Flushes standard output and standard error attached to `rt`.
///
/// # Safety
/// `rt` must be null or a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_flush(rt: *mut NoevRuntime) -> bool {
    let Some(rt) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    let flush_err = if let Some(output) = &rt.output {
        let mut output = output.borrow_mut();
        output
            .stdout
            .flush()
            .and_then(|()| output.stderr.flush())
            .err()
    } else {
        None
    };
    if let Some(e) = flush_err {
        return rt.fail(RuntimeError::new(format!("cannot write output: {e}")));
    }
    true
}

/// Draws the next deterministic seed from `rt`'s PRNG.
///
/// # Safety
/// `rt` must be null or a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_next_seed(rt: *mut NoevRuntime) -> u64 {
    unsafe { rt_mut(rt) }.map_or(0, NoevRuntime::next_seed)
}

/// Whether any measurement has occurred on `rt`.
///
/// # Safety
/// `rt` must be null or a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_measured(rt: *const NoevRuntime) -> bool {
    unsafe { rt_ref(rt) }.is_some_and(|rt| rt.measured)
}

// ---------------------------------------------------------------------------------------
// Qubits
// ---------------------------------------------------------------------------------------

/// Allocates a qubit in \|0⟩, writing its handle to `out`.
///
/// # Safety
/// `rt` and `out` must be valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_qubit_allocate(rt: *mut NoevRuntime, out: *mut NoevQubit) -> bool {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    if out.is_null() {
        return rt_ref.fail(RuntimeError::new("null output pointer in qubit_allocate"));
    }
    match rt_ref.allocate_inner() {
        Ok(q) => {
            unsafe { *out = NoevQubit(q) };
            true
        }
        Err(err) => rt_ref.fail(err),
    }
}

/// Releases `q`, which must be in \|0⟩ (checked by simulators, `spec/ownership.md` §11).
///
/// # Safety
/// `rt` must be a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_qubit_release(rt: *mut NoevRuntime, q: NoevQubit) -> bool {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    let res = rt_ref.release_inner(q.0);
    gate_result(rt, res)
}

/// Resets `q` to \|0⟩.
///
/// # Safety
/// `rt` must be a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_qubit_reset(rt: *mut NoevRuntime, q: NoevQubit) -> bool {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    let res = rt_ref.reset_inner(q.0);
    gate_result(rt, res)
}

// ---------------------------------------------------------------------------------------
// Core gates (QIR-shaped)
// ---------------------------------------------------------------------------------------

macro_rules! unit_gate {
    ($fn_name:ident, $gate:ident, $adj:expr, $doc:expr) => {
        #[doc = $doc]
        ///
        /// # Safety
        /// `rt` must be a valid [`NoevRuntime`] pointer.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $fn_name(rt: *mut NoevRuntime, q: NoevQubit) -> bool {
            let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
                return false;
            };
            let res = rt_ref.emit_gate(Gate::$gate, $adj, Vec::new(), vec![q.0]);
            gate_result(rt, res)
        }
    };
}

macro_rules! angle_gate {
    ($fn_name:ident, $gate:ident, $doc:expr) => {
        #[doc = $doc]
        ///
        /// # Safety
        /// `rt` must be a valid [`NoevRuntime`] pointer.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $fn_name(rt: *mut NoevRuntime, theta: f64, q: NoevQubit) -> bool {
            let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
                return false;
            };
            let res = rt_ref.emit_gate(Gate::$gate, false, vec![theta], vec![q.0]);
            gate_result(rt, res)
        }
    };
}

unit_gate!(noev_qis_x, X, false, "Applies Pauli-X to `q`.");
unit_gate!(noev_qis_y, Y, false, "Applies Pauli-Y to `q`.");
unit_gate!(noev_qis_z, Z, false, "Applies Pauli-Z to `q`.");
unit_gate!(noev_qis_h, H, false, "Applies Hadamard to `q`.");
unit_gate!(noev_qis_s, S, false, "Applies S to `q`.");
unit_gate!(noev_qis_s_adj, S, true, "Applies S† to `q`.");
unit_gate!(noev_qis_t, T, false, "Applies T to `q`.");
unit_gate!(noev_qis_t_adj, T, true, "Applies T† to `q`.");

angle_gate!(noev_qis_rx, Rx, "Applies Rx(theta) to `q`.");
angle_gate!(noev_qis_ry, Ry, "Applies Ry(theta) to `q`.");
angle_gate!(noev_qis_rz, Rz, "Applies Rz(theta) to `q`.");
angle_gate!(noev_qis_p, P, "Applies phase gate P(theta) to `q`.");

/// Applies U(theta, phi, lam) to `q`.
///
/// # Safety
/// `rt` must be a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_qis_u(
    rt: *mut NoevRuntime,
    theta: f64,
    phi: f64,
    lam: f64,
    q: NoevQubit,
) -> bool {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    let res = rt_ref.emit_gate(Gate::U, false, vec![theta, phi, lam], vec![q.0]);
    gate_result(rt, res)
}

/// Applies U(theta, phi, lam)† to `q`.
///
/// # Safety
/// `rt` must be a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_qis_u_adj(
    rt: *mut NoevRuntime,
    theta: f64,
    phi: f64,
    lam: f64,
    q: NoevQubit,
) -> bool {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    let res = rt_ref.emit_gate(Gate::U, true, vec![theta, phi, lam], vec![q.0]);
    gate_result(rt, res)
}

/// Applies SWAP to `q1` and `q2`.
///
/// # Safety
/// `rt` must be a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_qis_swap(rt: *mut NoevRuntime, q1: NoevQubit, q2: NoevQubit) -> bool {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    let res = rt_ref.emit_gate(Gate::Swap, false, Vec::new(), vec![q1.0, q2.0]);
    gate_result(rt, res)
}

/// Applies an arbitrary one- or two-qubit unitary matrix (`params` = interleaved real/imag
/// entries, row-major) to `targets`.
///
/// # Safety
/// `rt` must be valid; `targets` must point to `n_targets` qubits and `params` to `n_params`
/// `f64` values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_qis_unitary(
    rt: *mut NoevRuntime,
    targets: *const NoevQubit,
    n_targets: usize,
    params: *const f64,
    n_params: usize,
    adjoint: bool,
) -> bool {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    let targets = if n_targets == 0 || targets.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(targets, n_targets) }
            .iter()
            .map(|q| q.0)
            .collect()
    };
    let params = if n_params == 0 || params.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(params, n_params) }.to_vec()
    };
    let res = rt_ref.emit_gate(Gate::Unitary, adjoint, params, targets);
    gate_result(rt, res)
}

// ---------------------------------------------------------------------------------------
// Controls, adjoint, and `with`/`do`
// ---------------------------------------------------------------------------------------

/// Pushes `len` control qubits onto the active `ctrl` stack.
///
/// # Safety
/// `rt` must be valid; `controls` must point to `len` readable [`NoevQubit`]s when `len > 0`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_ctrl_push(
    rt: *mut NoevRuntime,
    controls: *const NoevQubit,
    len: usize,
) {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return;
    };
    rt_ref.control_marks.push(rt_ref.controls.len());
    if !controls.is_null() && len > 0 {
        let slice = unsafe { std::slice::from_raw_parts(controls, len) };
        rt_ref.controls.extend(slice.iter().map(|q| q.0));
    }
}

/// Pops the most recent `ctrl` block's controls.
///
/// # Safety
/// `rt` must be a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_ctrl_pop(rt: *mut NoevRuntime) {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return;
    };
    let mark = rt_ref.control_marks.pop().unwrap_or(0);
    rt_ref.controls.truncate(mark);
}

/// Returns the current control mark depth, for restoring on early `return` from `ctrl`.
///
/// # Safety
/// `rt` must be null or valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_ctrl_depth(rt: *const NoevRuntime) -> usize {
    unsafe { rt_ref(rt) }.map_or(0, |rt| rt.control_marks.len())
}

/// Restores the control stack to a previous `depth` returned by [`noev_rt_ctrl_depth`].
///
/// # Safety
/// `rt` must be a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_ctrl_restore(rt: *mut NoevRuntime, depth: usize) {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return;
    };
    while rt_ref.control_marks.len() > depth {
        let mark = rt_ref.control_marks.pop().unwrap_or(0);
        rt_ref.controls.truncate(mark);
    }
}

/// Begins recording operations for an `adj` call.
///
/// # Safety
/// `rt` must be a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_adj_start(rt: *mut NoevRuntime) {
    if let Some(rt_ref) = unsafe { rt_mut(rt) } {
        rt_ref.record_ops_start();
    }
}

/// Finishes an `adj` call, replaying recorded operations in reverse and inverted, then
/// verifying any deferred qubit releases.
///
/// # Safety
/// `rt` must be a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_adj_end(rt: *mut NoevRuntime) -> bool {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    let res = rt_ref.adj_end_inner();
    gate_result(rt, res)
}

/// Discards an in-progress `adj` recording frame when the inverted call fails.
///
/// # Safety
/// `rt` must be a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_adj_abort(rt: *mut NoevRuntime) {
    if let Some(rt_ref) = unsafe { rt_mut(rt) } {
        rt_ref.recording.pop();
        rt_ref.adj_releases.pop();
    }
}

/// Enters the `with` compute block of `with { … } do { … }` (suspending outer `ctrl`).
///
/// # Safety
/// `rt` must be a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_with_start(rt: *mut NoevRuntime) {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return;
    };
    let controls = std::mem::take(&mut rt_ref.controls);
    let control_marks = std::mem::take(&mut rt_ref.control_marks);
    rt_ref.with_active.push(WithFrame {
        ops: Vec::new(),
        allocs: Vec::new(),
        controls,
        control_marks,
    });
}

/// Transitions from `with` to `do` (restoring outer `ctrl` for `do`).
///
/// # Safety
/// `rt` must be a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_with_do(rt: *mut NoevRuntime) {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return;
    };
    if let Some(mut frame) = rt_ref.with_active.pop() {
        rt_ref.controls = std::mem::take(&mut frame.controls);
        rt_ref.control_marks = std::mem::take(&mut frame.control_marks);
        rt_ref.with_pending.push(frame);
    }
}

/// Finishes `with`/`do`: uncomputes `with`'s recorded operations in reverse (without outer
/// `ctrl`) and releases ancillas allocated in `with`.
///
/// # Safety
/// `rt` must be a valid [`NoevRuntime`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_with_end(rt: *mut NoevRuntime) -> bool {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    let res = rt_ref.with_end_inner();
    gate_result(rt, res)
}

// ---------------------------------------------------------------------------------------
// Measurement and result handles
// ---------------------------------------------------------------------------------------

/// Measures `q` in the Z basis, consuming and freeing it, and writes its [`NoevResult`] to
/// `out`.
///
/// # Safety
/// `rt` and `out` must be valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_qis_mz(
    rt: *mut NoevRuntime,
    q: NoevQubit,
    out: *mut NoevResult,
) -> bool {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    if out.is_null() {
        return rt_ref.fail(RuntimeError::new("null output pointer in mz"));
    }
    match rt_ref.measure_inner(q.0) {
        Ok(result) => {
            unsafe { *out = result };
            true
        }
        Err(err) => rt_ref.fail(err),
    }
}

/// Whether `res` is a recorded circuit classical bit handle rather than a concrete bit.
#[unsafe(no_mangle)]
pub extern "C" fn noev_rt_result_is_clbit(res: NoevResult) -> bool {
    res.is_clbit()
}

/// The classical bit index of a recorded measurement handle.
#[unsafe(no_mangle)]
pub extern "C" fn noev_rt_result_clbit_index(res: NoevResult) -> usize {
    res.clbit_index()
}

/// Reads `res` as `0` or `1`: returns the concrete bit directly, or in circuit-recording
/// mode reads the bit from the replay outcome (marking `needs_values` when recording).
///
/// # Safety
/// `rt` must be null or valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_result_read(rt: *mut NoevRuntime, res: NoevResult) -> u8 {
    if !res.is_clbit() {
        return u8::from(res.as_bit());
    }
    let idx = res.clbit_index();
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return 0;
    };
    rt_ref.needs_values.set(true);
    match &rt_ref.mode {
        Mode::Build(recording) => match recording.known {
            Some(bits) if idx < 64 => ((bits >> idx) & 1) as u8,
            _ => 0,
        },
        Mode::Simulate => 0,
    }
}

// ---------------------------------------------------------------------------------------
// State inspection and runs
// ---------------------------------------------------------------------------------------

/// Reads the statevector amplitudes of `qubits` into `out` (`out_len` must be `1 << n_qubits`).
///
/// # Safety
/// `rt` must be valid; `qubits` must point to `n_qubits` handles; `out` must point to
/// `out_len` writable [`NoevComplex`] values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_amplitudes(
    rt: *mut NoevRuntime,
    qubits: *const NoevQubit,
    n_qubits: usize,
    out: *mut NoevComplex,
    out_len: usize,
) -> bool {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    if matches!(rt_ref.mode, Mode::Build(_)) {
        return rt_ref.fail(RuntimeError::new(
            "amplitudes can be read only while a simulator runs gate by gate",
        ));
    }
    let qs: Vec<usize> = if n_qubits == 0 || qubits.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(qubits, n_qubits) }
            .iter()
            .map(|q| q.0)
            .collect()
    };
    let name = rt_ref.backend.get().name().to_string();
    let session = match rt_ref.session_mut() {
        Ok(s) => s,
        Err(err) => return rt_ref.fail(err),
    };
    let Some(amps) = session.amplitudes(&qs) else {
        return rt_ref.fail(RuntimeError::new(format!(
            "`{name}` cannot show amplitudes: it does not hold the state"
        )));
    };
    let amps = match amps {
        Ok(amps) => amps,
        Err(err) => return rt_ref.fail(backend_error(err)),
    };
    if out.is_null() || out_len < amps.len() {
        return rt_ref.fail(RuntimeError::new("output buffer too small for amplitudes"));
    }
    let dst = unsafe { std::slice::from_raw_parts_mut(out, amps.len()) };
    for (slot, (re, im)) in dst.iter_mut().zip(amps) {
        *slot = NoevComplex { re, im };
    }
    true
}

/// Reads the amplitude of the all-`0` basis state into `out`.
///
/// # Safety
/// `rt` and `out` must be valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_zero_amplitude(
    rt: *mut NoevRuntime,
    out: *mut NoevComplex,
) -> bool {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    if out.is_null() {
        return rt_ref.fail(RuntimeError::new("null output pointer in zero_amplitude"));
    }
    let session = match rt_ref.session_mut() {
        Ok(s) => s,
        Err(err) => return rt_ref.fail(err),
    };
    match session.zero_amplitude() {
        Some(Ok((re, im))) => {
            unsafe { *out = NoevComplex { re, im } };
            true
        }
        Some(Err(err)) => rt_ref.fail(backend_error(err)),
        None => rt_ref.fail(RuntimeError::new("the simulator cannot read amplitudes")),
    }
}

/// Computes the exact expectation value of a Pauli sum on `rt`'s current state.
///
/// # Safety
/// `rt` and `out` must be valid; `terms` must point to `n_terms` valid [`NoevPauliTerm`]s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_expectation_exact(
    rt: *mut NoevRuntime,
    terms: *const NoevPauliTerm,
    n_terms: usize,
    out: *mut f64,
) -> bool {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    if out.is_null() {
        return rt_ref.fail(RuntimeError::new(
            "null output pointer in expectation_exact",
        ));
    }
    let raw_terms = if n_terms == 0 || terms.is_null() {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(terms, n_terms) }
    };
    let mut parsed: Vec<(f64, Vec<(usize, Pauli)>)> = Vec::with_capacity(raw_terms.len());
    for term in raw_terms {
        let mut factors = Vec::with_capacity(term.len);
        if term.len > 0 && !term.factors.is_null() {
            for f in unsafe { std::slice::from_raw_parts(term.factors, term.len) } {
                let pauli = match f.pauli {
                    0 => Pauli::X,
                    1 => Pauli::Y,
                    2 => Pauli::Z,
                    other => {
                        return rt_ref
                            .fail(RuntimeError::new(format!("invalid Pauli tag {other}")));
                    }
                };
                factors.push((f.qubit.0, pauli));
            }
        }
        parsed.push((term.coeff, factors));
    }
    let name = rt_ref.backend.get().name().to_string();
    let Some(session) = rt_ref.session.as_ref() else {
        return rt_ref.fail(RuntimeError::new(format!(
            "`{name}` cannot compute expectation values yet"
        )));
    };
    match session.expectation(&parsed) {
        Some(Ok(val)) => {
            unsafe { *out = val };
            true
        }
        Some(Err(err)) => rt_ref.fail(backend_error(err)),
        None => rt_ref.fail(RuntimeError::new(format!(
            "`{name}` cannot compute expectation values yet"
        ))),
    }
}

/// Runs a shot body `shots` times on `rt`'s backend, using circuit recording + replay when
/// `is_circuit` is true and the backend accepts circuits, or shot by shot otherwise.
///
/// # Safety
/// `rt` must be valid; `body` and `on_shot` must be valid C callbacks with their environments.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_sample_run(
    rt: *mut NoevRuntime,
    shots: usize,
    is_circuit: bool,
    body: NoevRunFn,
    env: *mut c_void,
    on_shot: NoevShotCallback,
    cb_env: *mut c_void,
) -> bool {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    if shots == 0 {
        return rt_ref.fail(RuntimeError::new("`sample` needs at least one shot"));
    }
    let backend_ptr = rt_ref.backend.as_ptr();
    let seed = rt_ref.next_seed();
    let mut seeds = SmallRng::seed_from_u64(seed);

    if is_circuit {
        let mut builder = unsafe { NoevRuntime::new_from_ptr(backend_ptr, seeds.random()) };
        builder.set_mode(Mode::Build(Recording::default()));
        if body(&mut builder, env) {
            let Mode::Build(recording) = std::mem::take(&mut builder.mode) else {
                unreachable!("builder records");
            };
            let circuit = recording.circuit;
            if circuit.clbits <= 64
                && let Some(res) =
                    unsafe { &*backend_ptr }.run_circuit(&circuit, shots, seeds.random())
            {
                let counts = match res {
                    Ok(c) => c,
                    Err(e) => return rt_ref.fail(backend_error(e)),
                };
                if !unsafe { &*backend_ptr }.executes() {
                    return true;
                }
                for (bits, count) in counts {
                    let mut replay =
                        unsafe { NoevRuntime::new_from_ptr(backend_ptr, seeds.random()) };
                    replay.set_mode(Mode::Build(Recording::knowing(bits)));
                    if !body(&mut replay, env) {
                        let err = replay.take_error();
                        return rt_ref.fail(err);
                    }
                    if !on_shot(&mut replay, count, cb_env) {
                        let err = replay.take_error();
                        return rt_ref.fail(err);
                    }
                }
                return true;
            }
        }
    }

    if !unsafe { &*backend_ptr }.executes() {
        return true;
    }

    for _ in 0..shots {
        let mut shot_rt = unsafe { NoevRuntime::new_from_ptr(backend_ptr, seeds.random()) };
        if !body(&mut shot_rt, env) {
            let err = shot_rt.take_error();
            return rt_ref.fail(err);
        }
        if !on_shot(&mut shot_rt, 1, cb_env) {
            let err = shot_rt.take_error();
            return rt_ref.fail(err);
        }
    }
    true
}

// ---------------------------------------------------------------------------------------
// Output and failure
// ---------------------------------------------------------------------------------------

/// Writes `len` UTF-8 bytes to `rt`'s standard output or standard error.
///
/// # Safety
/// `rt` must be valid; `bytes` must point to `len` readable bytes when `len > 0`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_write(
    rt: *mut NoevRuntime,
    stderr: bool,
    bytes: *const u8,
    len: usize,
) -> bool {
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    if len == 0 || bytes.is_null() {
        return true;
    }
    let slice = unsafe { std::slice::from_raw_parts(bytes, len) };
    let write_err = if let Some(output) = &rt_ref.output {
        let mut output = output.borrow_mut();
        let stream = if stderr {
            &mut output.stderr
        } else {
            &mut output.stdout
        };
        stream.write_all(slice).err()
    } else {
        None
    };
    if let Some(e) = write_err {
        return rt_ref.fail(RuntimeError::new(format!("cannot write output: {e}")));
    }
    true
}

/// Checks an assertion; when `cond` is false, records an error on `rt` and returns `false`.
///
/// # Safety
/// `rt` must be valid; `msg` may be null or point to `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_assert(
    rt: *mut NoevRuntime,
    cond: bool,
    msg: *const u8,
    len: usize,
) -> bool {
    if cond {
        return true;
    }
    let Some(rt_ref) = (unsafe { rt_mut(rt) }) else {
        return false;
    };
    let text = if msg.is_null() || len == 0 {
        String::from("assertion failed")
    } else {
        let slice = unsafe { std::slice::from_raw_parts(msg, len) };
        String::from_utf8_lossy(slice).into_owned()
    };
    rt_ref.fail(RuntimeError::new(text))
}
