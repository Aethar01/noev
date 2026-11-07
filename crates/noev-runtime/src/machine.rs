//! Runs IR, sending quantum operations to a [`Backend`].
//!
//! One [`Machine`] runs one shot. Qubits are allocated on demand: the backend adds one
//! qubit when no released qubit is free, and qubits that are measured, discarded or released
//! go back on a free list in \|0⟩ for reuse.
//!
//! `ctrl` keeps a stack of control qubits that every gate picks up. `adj` records the gates a
//! reversible call applies, then applies their inverses in reverse order. Reversible code
//! cannot measure or allocate, and its classical computation is deterministic, so this is
//! exactly the inverse.

use noev_ir::ir::*;
use noev_sema::prelude::{DistMethod, FloatConst, Gate, Math, Pauli as IrPauli};
use noev_sema::{Ty, TyKind};
use noev_syntax::Span;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use crate::backend::{Backend, BackendError, Circuit, Condition, Instruction, Op, Session};
use crate::format;
use crate::int::Int;
use crate::sized;
use crate::value::{Observable, Pauli, Pointer, Value};

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

type Result<T> = std::result::Result<T, RuntimeError>;

/// The standard error of an estimated expectation value without `within` (`spec/types.md`
/// §3).
const DEFAULT_PRECISION: f64 = 0.01;

/// The most runs one expectation value may take, so a tiny precision fails clearly instead of
/// running for ever.
const MAX_RUNS: f64 = 1e9;

/// How many runs give a standard error of at most `precision` for a sum of ±1 averages whose
/// coefficients sum to `weight` in absolute value, when this part of it has `part`: with
/// `part × weight / precision²` runs per part, the variances add up to `precision²`.
fn runs_for(part: f64, weight: f64, precision: f64) -> Result<usize> {
    let runs = (part * weight / (precision * precision)).ceil().max(1.0);
    if runs > MAX_RUNS {
        return Err(RuntimeError::new(format!(
            "an expectation value within {precision} would take {runs:.0} runs; ask for less precision"
        )));
    }
    Ok(runs as usize)
}

/// How a machine treats quantum operations.
#[derive(Clone, Debug, Default)]
pub enum Mode {
    /// Send them to the backend, one at a time.
    #[default]
    Simulate,
    /// Record them into a circuit instead, without simulating. Measurement results are the
    /// circuit's classical bits ([`Value::Clbit`]); where a known value is needed, they are
    /// taken from the recording's `known` outcome, or are a placeholder `0`. Only used for runs
    /// that are circuits, where a measurement result cannot affect anything before the last
    /// quantum operation except through classical control in the circuit.
    Build(Recording),
}

/// A circuit being recorded in [`Mode::Build`].
#[derive(Clone, Debug, Default)]
pub struct Recording {
    /// The circuit so far.
    pub circuit: Circuit,
    /// Released and measured qubits, and whether each needs a reset before reuse.
    free: Vec<(usize, bool)>,
    /// One outcome of the circuit, classical bit `k` at bit `k`, when re-running its classical
    /// code for that outcome. Recording the same way again keeps the bits numbered the same.
    pub known: Option<u64>,
    /// How many recorded loops enclose the current point: a qubit allocated in a loop's body
    /// may have been used in the previous pass, so it is reset.
    loops: u32,
    /// The conditions the enclosing recorded branches decided, and how: inside
    /// `if (c[0] == 0)`, testing `c[0] == 0` again needs no `if` of its own.
    decided: Vec<(Condition, bool)>,
    /// When recording `main` as a one-shot circuit: the results of the repeated runs it
    /// starts (`sample`, expectation values), in order. They are computed while recording and
    /// reused when its classical code runs for the outcome, so each runs once.
    pub runs: Option<std::collections::VecDeque<Value>>,
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

impl Machine<'_> {
    /// The result of a repeated run: reused when re-running `main`'s classical code for its
    /// outcome, otherwise computed by `compute` (and kept, while recording `main`).
    fn repeated_run(&mut self, compute: impl FnOnce(&mut Self) -> Result<Value>) -> Result<Value> {
        let replaying = matches!(
            &self.mode,
            Mode::Build(Recording {
                known: Some(_),
                runs: Some(_),
                ..
            })
        );
        if replaying
            && let Mode::Build(Recording {
                runs: Some(runs), ..
            }) = &mut self.mode
        {
            return runs
                .pop_front()
                .ok_or_else(|| RuntimeError::new("`main` started a run it did not start before"));
        }
        let value = compute(self)?;
        if let Mode::Build(Recording {
            runs: Some(runs), ..
        }) = &mut self.mode
        {
            runs.push_back(value.clone());
        }
        Ok(value)
    }
}

/// Recorded circuits use fresh qubits up to this many, so their measurements can all come at
/// the end; beyond it they reuse measured qubits after a reset.
const FRESH_QUBITS: usize = 24;

/// The state of one run of a program.
pub struct Machine<'p> {
    program: &'p Program,
    backend: &'p dyn Backend,
    /// The backend's gate-level session; `None` if it only runs circuits.
    session: Option<Box<dyn Session>>,
    /// Qubits in \|0⟩ that can be handed out again.
    free: Vec<usize>,
    frames: Vec<Vec<Value>>,
    /// Control qubits that every gate picks up, from enclosing `ctrl` blocks.
    controls: Vec<usize>,
    /// The length of `controls` before each `ctrl` push.
    control_marks: Vec<usize>,
    /// Gates recorded by enclosing `adj` calls, innermost last.
    recording: Vec<Vec<Op>>,
    rng: SmallRng,
    /// Whether anything has been measured, which makes an expectation value an estimate.
    measured: bool,
    /// Where `print` and `eprint` write; `None` in runs that are repeated, which cannot print.
    output: Option<Rc<RefCell<Output>>>,
    /// Simulating, or recording a circuit (possibly for one known outcome).
    pub mode: Mode,
    /// Whether, while recording, an operation needed a measurement result's actual value.
    /// If not, a circuit's results are its classical bits in place, and outcomes need no
    /// replay.
    pub needs_values: std::cell::Cell<bool>,
}

impl<'p> Machine<'p> {
    /// A machine with no qubits, running on `backend`.
    pub fn new(program: &'p Program, backend: &'p dyn Backend, seed: u64) -> Self {
        let mut seeds = SmallRng::seed_from_u64(seed);
        Machine {
            program,
            backend,
            session: backend.session(seeds.random()),
            free: Vec::new(),
            frames: Vec::new(),
            controls: Vec::new(),
            control_marks: Vec::new(),
            recording: Vec::new(),
            rng: seeds,
            measured: false,
            output: None,
            mode: Mode::Simulate,
            needs_values: std::cell::Cell::new(false),
        }
    }

    /// A machine in `mode`.
    pub fn with_mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    /// Sends the program's output to `output`.
    pub fn with_output(mut self, output: Rc<RefCell<Output>>) -> Self {
        self.output = Some(output);
        self
    }

    /// Whether anything has been measured, so later shots could behave differently.
    pub fn measured(&self) -> bool {
        self.measured
    }

    /// Measures every qubit in `value`, replacing each with its `Bit`. This is how returned
    /// qubits are consumed at the end of a shot (`spec/core.md` §6).
    pub fn measure_value(&mut self, value: Value) -> Result<Value> {
        Ok(match value {
            Value::Qubit(q) => self.measurement(q)?,
            Value::Aggregate(items) => Value::Aggregate(
                items
                    .into_iter()
                    .map(|v| self.measure_value(v))
                    .collect::<Result<_>>()?,
            ),
            other => other,
        })
    }

    /// Samples the current state `shots` times, returning how often each outcome of `qubits`
    /// occurred, with `qubits[k]` at bit `k`. Leaves the state unchanged. `None` if the
    /// backend cannot (only simulators can).
    pub fn sample(&mut self, qubits: &[usize], shots: usize) -> Option<Vec<(u64, usize)>> {
        self.session.as_mut()?.sample(qubits, shots)
    }

    /// The backend's gate-level session, for running operations one at a time.
    fn session(&mut self) -> Result<&mut dyn Session> {
        let name = self.backend.name();
        let executes = self.backend.executes();
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

    /// Calls `func` with `args` and returns its result.
    pub fn call(&mut self, func: FuncId, args: Vec<Value>) -> Result<Value> {
        let program = self.program;
        let function = &program.functions[func.index()];
        let mut locals: Vec<Value> = function.locals.iter().map(|l| self.default(l.ty)).collect();
        for (i, arg) in args.into_iter().enumerate() {
            locals[i + 1] = arg;
        }
        self.frames.push(locals);
        let control_depth = (self.controls.len(), self.control_marks.len());
        let result = self.run_blocks(function, 0, None);
        // A `return` from inside a `ctrl` block leaves its controls behind.
        self.controls.truncate(control_depth.0);
        self.control_marks.truncate(control_depth.1);
        let mut frame = self.frames.pop().expect("pushed above");
        result?;
        Ok(std::mem::replace(&mut frame[0], Value::Unit))
    }

    /// Runs `function`'s blocks from `start` until it returns, or until it reaches `stop`.
    fn run_blocks(
        &mut self,
        function: &'p Function,
        start: usize,
        stop: Option<usize>,
    ) -> Result<()> {
        let mut block = start;
        loop {
            if Some(block) == stop {
                return Ok(());
            }
            let current = &function.blocks[block];
            for (stmt, &span) in current.stmts.iter().zip(&current.spans) {
                self.stmt(stmt).map_err(|e| e.at(span))?;
            }
            block = match &current.term {
                Terminator::Goto(next) => next.index(),
                Terminator::Branch {
                    cond,
                    then,
                    otherwise,
                    control: Some(control),
                } if matches!(self.mode, Mode::Build(_)) => {
                    match self.record_control(function, block, cond, *then, *otherwise, control)? {
                        Some(next) => next.index(),
                        // Not on a measurement result here: an ordinary branch.
                        None if self.truth(cond)? => then.index(),
                        None => otherwise.index(),
                    }
                }
                Terminator::Branch {
                    cond,
                    then,
                    otherwise,
                    ..
                } => {
                    if self.truth(cond)? {
                        then.index()
                    } else {
                        otherwise.index()
                    }
                }
                Terminator::Call {
                    func,
                    args,
                    dest,
                    adjoint,
                    next,
                } => {
                    let args = args
                        .iter()
                        .map(|a| self.operand(a))
                        .collect::<Result<Vec<_>>>()?;
                    let value = if *adjoint {
                        self.recording.push(Vec::new());
                        let value = self.call(*func, args);
                        let ops = self.recording.pop().expect("pushed above");
                        let value = value?;
                        for mut op in ops.into_iter().rev() {
                            op.adjoint = !op.adjoint;
                            self.execute(op)?;
                        }
                        value
                    } else {
                        self.call(*func, args)?
                    };
                    self.store(dest, value)?;
                    next.index()
                }
                Terminator::Assert { cond, next, span } => {
                    if !self.truth(cond)? {
                        return Err(RuntimeError {
                            span: Some(*span),
                            ..RuntimeError::new("assertion failed")
                        });
                    }
                    next.index()
                }
                Terminator::Return => return Ok(()),
                // A recorded branch that no shot takes: a `match`'s last test failing.
                Terminator::Unreachable
                    if stop.is_some() && matches!(self.mode, Mode::Build(_)) =>
                {
                    return Ok(());
                }
                Terminator::Unreachable => {
                    return Err(RuntimeError::new(format!(
                        "reached unreachable code in `{}`",
                        function.name
                    )));
                }
            };
        }
    }

    /// Records an `if` or `while` on a measurement result as classical control in the circuit
    /// (`Control`), returning where to continue; `None` if its condition does not depend on a
    /// measurement result in this call, so it is an ordinary branch.
    ///
    /// The variables it changes live in classical bits: assigned at the end of each branch,
    /// or before a loop and at the end of each pass. An `if` records both branches, each up
    /// to where they join; a `while` records its body once, up to where it returns to the
    /// condition.
    fn record_control(
        &mut self,
        function: &'p Function,
        head: usize,
        cond: &Operand,
        then: Block,
        otherwise: Block,
        control: &Control,
    ) -> Result<Option<Block>> {
        let carried: Vec<Place> = control.carried.iter().map(|&l| Place::local(l)).collect();
        if !control.repeats {
            let Some(condition) = self.operand(cond)?.condition() else {
                return Ok(None);
            };
            if let Some(known) = self.decided(&condition) {
                return Ok(Some(if known { then } else { otherwise }));
            }
            let before = self.load_all(&carried)?;
            let free = self.recording().free.clone();
            self.recording().decided.push((condition.clone(), true));
            let then_result = self.record_branch(function, then, control.join, &carried);
            self.recording().decided.pop();
            let (mut then_steps, then_values) = then_result?;
            let then_free = std::mem::replace(&mut self.recording().free, free);
            for (place, value) in carried.iter().zip(&before) {
                self.store(place, value.clone())?;
            }
            self.recording().decided.push((condition.clone(), false));
            let else_result = self.record_branch(function, otherwise, control.join, &carried);
            self.recording().decided.pop();
            let (mut else_steps, else_values) = else_result?;
            // A qubit either branch freed is free, and may need a reset.
            let recording = self.recording();
            for (qubit, dirty) in then_free {
                match recording.free.iter_mut().find(|(q, _)| *q == qubit) {
                    Some(entry) => entry.1 |= dirty,
                    None => recording.free.push((qubit, dirty)),
                }
            }
            for (i, place) in carried.iter().enumerate() {
                if then_values[i] == before[i] && else_values[i] == before[i] {
                    continue;
                }
                let (bit, clbit) = self.new_clbit(&before[i]);
                then_steps.push(assign(clbit, &then_values[i])?);
                else_steps.push(assign(clbit, &else_values[i])?);
                self.store(place, bit)?;
            }
            self.recording().circuit.instructions.push(Instruction::If {
                condition,
                then: then_steps,
                otherwise: else_steps,
            });
            return Ok(Some(control.join));
        }

        // A loop: each variable it changes moves into a classical bit first.
        let mut bits = Vec::with_capacity(carried.len());
        for place in &carried {
            let value = self.load(place)?;
            let (bit, clbit) = self.new_clbit(&value);
            let step = assign(clbit, &value)?;
            self.recording().circuit.instructions.push(step);
            self.store(place, bit.clone())?;
            bits.push((bit, clbit));
        }
        // The condition again, now in terms of those bits.
        let block = &function.blocks[head];
        for (stmt, &span) in block.stmts.iter().zip(&block.spans) {
            self.stmt(stmt).map_err(|e| e.at(span))?;
        }
        let Some(condition) = self.operand(cond)?.condition() else {
            return Ok(None);
        };
        let free = self.recording().free.clone();
        // Each pass decides its own conditions.
        let decided = std::mem::take(&mut self.recording().decided);
        self.recording().loops += 1;
        let result = self.record_branch(function, then, control.join, &carried);
        self.recording().loops -= 1;
        self.recording().decided = decided;
        let (mut body, values) = result?;
        for ((place, (bit, clbit)), value) in carried.iter().zip(&bits).zip(&values) {
            body.push(assign(*clbit, value)?);
            self.store(place, bit.clone())?;
        }
        // The loop may run no passes, so what was free still is; everything it freed may need
        // a reset.
        let recording = self.recording();
        for (qubit, _) in free {
            if !recording.free.iter().any(|(q, _)| *q == qubit) {
                recording.free.push((qubit, true));
            }
        }
        for entry in &mut recording.free {
            entry.1 = true;
        }
        recording
            .circuit
            .instructions
            .push(Instruction::While { condition, body });
        // The condition's temporaries were computed for the bits, which is what the code after
        // the loop sees too.
        Ok(Some(otherwise))
    }

    /// How an enclosing recorded branch decided `condition` (or its negation), if one did.
    fn decided(&mut self, condition: &Condition) -> Option<bool> {
        self.recording()
            .decided
            .iter()
            .rev()
            .find_map(|(c, value)| {
                if c == condition {
                    Some(*value)
                } else if Condition::negate(c.clone()) == *condition {
                    Some(!*value)
                } else {
                    None
                }
            })
    }

    /// Records the blocks from `start` up to `stop` into their own list of steps, returning
    /// them and the values of the carried variables at the end.
    fn record_branch(
        &mut self,
        function: &'p Function,
        start: Block,
        stop: Block,
        carried: &[Place],
    ) -> Result<(Vec<Instruction>, Vec<Value>)> {
        let outer = std::mem::take(&mut self.recording().circuit.instructions);
        let result = self.run_blocks(function, start.index(), Some(stop.index()));
        let inner = std::mem::replace(&mut self.recording().circuit.instructions, outer);
        result?;
        Ok((inner, self.load_all(carried)?))
    }

    fn load_all(&self, places: &[Place]) -> Result<Vec<Value>> {
        places.iter().map(|p| self.load(p)).collect()
    }

    /// A new classical bit for a variable whose current value is `like`: the value standing
    /// for the bit, and its index.
    fn new_clbit(&mut self, like: &Value) -> (Value, usize) {
        let circuit = &mut self.recording().circuit;
        let clbit = circuit.clbits;
        circuit.clbits += 1;
        let is_bit = !matches!(like, Value::Bool(_) | Value::Cond(_, false));
        let value = if is_bit {
            Value::Clbit(clbit)
        } else {
            Value::Cond(Rc::new(Condition::Clbit(clbit)), false)
        };
        (value, clbit)
    }

    fn recording(&mut self) -> &mut Recording {
        match &mut self.mode {
            Mode::Build(recording) => recording,
            Mode::Simulate => unreachable!("only while recording"),
        }
    }

    /// The value a local starts with: aggregates are built out so their parts can be
    /// assigned one at a time.
    fn default(&self, ty: Ty) -> Value {
        let types = &self.program.types;
        match types.kind(ty) {
            TyKind::Tuple(elems) => {
                Value::Aggregate(elems.iter().map(|&t| self.default(t)).collect())
            }
            &TyKind::Array(elem, len) => {
                Value::Aggregate((0..len).map(|_| self.default(elem)).collect())
            }
            &TyKind::Struct(s) if types.structs[s.index()].variants.is_some() => {
                Value::Variant(0, Vec::new())
            }
            &TyKind::Struct(s) => Value::Aggregate(
                types.structs[s.index()]
                    .fields
                    .iter()
                    .map(|&(_, t)| self.default(t))
                    .collect(),
            ),
            _ => Value::Unit,
        }
    }
}

// Places and values.

impl Machine<'_> {
    fn resolve(&self, place: &Place) -> Result<Pointer> {
        let mut ptr = Pointer {
            frame: self.frames.len() - 1,
            local: place.local.index(),
            path: Vec::new(),
        };
        for proj in &place.proj {
            match proj {
                Proj::Deref => match self.get(&ptr)? {
                    Value::Ref(target) => ptr = target.clone(),
                    other => {
                        return Err(RuntimeError::new(format!(
                            "expected a reference, found {other:?}"
                        )));
                    }
                },
                Proj::Field(i) => ptr.path.push(*i as usize),
                Proj::Index(index) => {
                    let index = match self.value(index)? {
                        Value::Int(v) => v,
                        other => {
                            return Err(RuntimeError::new(format!(
                                "expected an Int, found {other:?}"
                            )));
                        }
                    };
                    let len = match self.get(&ptr)? {
                        Value::Aggregate(items) => items.len(),
                        other => {
                            return Err(RuntimeError::new(format!(
                                "expected an array, found {other:?}"
                            )));
                        }
                    };
                    let Some(index) = index.to_i64().filter(|&i| i >= 0 && (i as usize) < len)
                    else {
                        return Err(RuntimeError::new(format!(
                            "index {index} is out of range for an array of length {len}"
                        )));
                    };
                    ptr.path.push(index as usize);
                }
            }
        }
        Ok(ptr)
    }

    fn get(&self, ptr: &Pointer) -> Result<&Value> {
        let mut value = &self.frames[ptr.frame][ptr.local];
        for &step in &ptr.path {
            value = match value {
                Value::Aggregate(items) | Value::Variant(_, items) => items
                    .get(step)
                    .ok_or_else(|| RuntimeError::new("this variant does not hold that value"))?,
                other => return Err(RuntimeError::new(format!("cannot take part of {other:?}"))),
            };
        }
        Ok(value)
    }

    fn get_mut(&mut self, ptr: &Pointer) -> Result<&mut Value> {
        let mut value = &mut self.frames[ptr.frame][ptr.local];
        for &step in &ptr.path {
            value = match value {
                Value::Aggregate(items) | Value::Variant(_, items) => items
                    .get_mut(step)
                    .ok_or_else(|| RuntimeError::new("this variant does not hold that value"))?,
                other => return Err(RuntimeError::new(format!("cannot take part of {other:?}"))),
            };
        }
        Ok(value)
    }

    fn load(&self, place: &Place) -> Result<Value> {
        let ptr = self.resolve(place)?;
        self.get(&ptr).cloned()
    }

    fn store(&mut self, place: &Place, value: Value) -> Result<()> {
        let ptr = self.resolve(place)?;
        *self.get_mut(&ptr)? = value;
        Ok(())
    }

    fn operand(&self, op: &Operand) -> Result<Value> {
        match op {
            Operand::Copy(place) | Operand::Move(place) => self.load(place),
            Operand::Const(c) => Ok(match *c {
                Const::Unit => Value::Unit,
                Const::Int(v) => Value::Int(Int::from(v)),
                Const::Sized(v, w) => Value::Sized(v, w),
                Const::F32(v) => Value::F32(v),
                Const::Float(v) => Value::Float(v),
                Const::Bool(v) => Value::Bool(v),
                Const::Bit(v) => Value::Bit(v),
            }),
        }
    }

    /// An operand's value where an operation needs it: a recorded measurement result becomes
    /// a concrete placeholder, noting that outcomes will need replaying.
    fn value(&self, op: &Operand) -> Result<Value> {
        let value = self.operand(op)?;
        Ok(self.concrete(value))
    }

    fn concrete(&self, value: Value) -> Value {
        if let Mode::Build(Recording {
            known: Some(bits), ..
        }) = self.mode
        {
            return value.with_clbits(bits);
        }
        match value {
            Value::Clbit(_) | Value::Cond(_, true) => {
                self.needs_values.set(true);
                Value::Bit(false)
            }
            Value::Cond(_, false) => {
                self.needs_values.set(true);
                Value::Bool(false)
            }
            Value::Aggregate(items) if items.iter().any(Value::has_clbits) => {
                Value::Aggregate(items.into_iter().map(|v| self.concrete(v)).collect())
            }
            other => other,
        }
    }

    /// An `Int` where a machine-sized one is needed (an index, a count).
    fn int(&self, op: &Operand) -> Result<i64> {
        match self.value(op)? {
            Value::Int(v) => v
                .to_i64()
                .ok_or_else(|| RuntimeError::new(format!("{v} is too large here"))),
            other => Err(RuntimeError::new(format!(
                "expected an Int, found {other:?}"
            ))),
        }
    }

    fn float(&self, op: &Operand) -> Result<f64> {
        match self.value(op)? {
            Value::Float(v) => Ok(v),
            other => Err(RuntimeError::new(format!(
                "expected a Float, found {other:?}"
            ))),
        }
    }

    fn truth(&self, op: &Operand) -> Result<bool> {
        match self.value(op)? {
            Value::Bool(v) | Value::Bit(v) => Ok(v),
            other => Err(RuntimeError::new(format!(
                "expected a Bool or Bit, found {other:?}"
            ))),
        }
    }

    fn qubit(&self, place: &Place) -> Result<usize> {
        match self.load(place)? {
            Value::Qubit(q) => Ok(q),
            other => Err(RuntimeError::new(format!(
                "expected a qubit, found {other:?}"
            ))),
        }
    }
}

// Statements.

impl Machine<'_> {
    fn stmt(&mut self, stmt: &Stmt) -> Result<()> {
        match stmt {
            Stmt::Assign(place, value) => {
                let value = self.rvalue(value)?;
                self.store(place, value)
            }
            Stmt::Alloc(place) => {
                let qubit = self.allocate()?;
                self.store(place, Value::Qubit(qubit))
            }
            Stmt::Gate {
                gate,
                adjoint,
                qubits,
                params,
            } => {
                let targets = qubits
                    .iter()
                    .map(|q| self.qubit(q))
                    .collect::<Result<Vec<_>>>()?;
                let params = params
                    .iter()
                    .map(|p| self.float(p))
                    .collect::<Result<Vec<_>>>()?;
                let op = Op {
                    gate: *gate,
                    adjoint: *adjoint,
                    params,
                    controls: self.controls.clone(),
                    targets,
                };
                match self.recording.last_mut() {
                    Some(recording) => {
                        recording.push(op);
                        Ok(())
                    }
                    None => self.execute(op),
                }
            }
            Stmt::Measure { dest, qubit } => {
                let qubit = self.qubit(qubit)?;
                let value = self.measurement(qubit)?;
                self.store(dest, value)
            }
            Stmt::Reset(place) => {
                let qubit = self.qubit(place)?;
                match &mut self.mode {
                    Mode::Simulate => self.session()?.reset(qubit).map_err(backend_error),
                    Mode::Build(recording) => {
                        recording
                            .circuit
                            .instructions
                            .push(Instruction::Reset(qubit));
                        Ok(())
                    }
                }
            }
            Stmt::Release(place) => {
                let qubit = self.qubit(place)?;
                match &mut self.mode {
                    Mode::Simulate => {}
                    Mode::Build(recording) => {
                        recording.free.push((qubit, false));
                        return Ok(());
                    }
                }
                // Simulators check `release`'s claim (spec/ownership.md §11).
                self.session()?.release(qubit).map_err(backend_error)?;
                self.free.push(qubit);
                Ok(())
            }
            Stmt::CtrlPush(places) => {
                self.control_marks.push(self.controls.len());
                for place in places {
                    let mut qubits = Vec::new();
                    self.load(place)?.qubits(&mut qubits);
                    self.controls.extend(qubits);
                }
                Ok(())
            }
            Stmt::CtrlPop => {
                let mark = self.control_marks.pop().unwrap_or(0);
                self.controls.truncate(mark);
                Ok(())
            }
            Stmt::Sample {
                dest,
                func,
                args,
                shots,
                elem,
            } => {
                let dist = self.repeated_run(|m| {
                    let args = args
                        .iter()
                        .map(|a| m.value(a))
                        .collect::<Result<Vec<_>>>()?;
                    let shots = usize::try_from(m.int(shots)?).map_err(|_| {
                        RuntimeError::new("`sample` needs a positive number of shots")
                    })?;
                    let seed = m.rng.random();
                    let program = m.program;
                    let elem = *elem;
                    let dist = crate::sampling::sample(
                        program,
                        m.backend,
                        *func,
                        args,
                        shots,
                        seed,
                        |v| format::display(program, v, elem),
                    )?;
                    Ok(Value::Distribution(Rc::new(dist)))
                })?;
                self.store(dest, dist)
            }
            Stmt::Print { stderr, args } => {
                let mut line = String::new();
                for (i, (op, ty)) in args.iter().enumerate() {
                    if i > 0 {
                        line.push(' ');
                    }
                    line.push_str(&format::display(self.program, &self.value(op)?, *ty));
                }
                line.push('\n');
                if let Some(output) = &self.output {
                    let mut output = output.borrow_mut();
                    let stream = if *stderr {
                        &mut output.stderr
                    } else {
                        &mut output.stdout
                    };
                    stream
                        .write_all(line.as_bytes())
                        .map_err(|e| RuntimeError::new(format!("cannot write output: {e}")))?;
                }
                Ok(())
            }
            Stmt::Expectation {
                dest,
                prep,
                observable,
                precision,
            } => {
                let Value::Observable(observable) = self.operand(observable)? else {
                    return Err(RuntimeError::new("expected an observable"));
                };
                let precision = match precision {
                    Some(p) => self.float(p)?,
                    None => DEFAULT_PRECISION,
                };
                if !(precision > 0.0 && precision.is_finite()) {
                    return Err(RuntimeError::new(format!(
                        "an expectation value's precision must be positive, not {precision}"
                    )));
                }
                let prep_args = match prep {
                    Prep::Ket(_) => Vec::new(),
                    Prep::Call(_, args) => args
                        .iter()
                        .map(|a| self.operand(a))
                        .collect::<Result<Vec<_>>>()?,
                };
                let value = self.repeated_run(|m| {
                    let seed = m.rng.random();
                    let value = expectation(
                        m.program,
                        m.backend,
                        prep,
                        prep_args,
                        &observable,
                        precision,
                        seed,
                    )?;
                    Ok(Value::Float(value))
                })?;
                self.store(dest, value)
            }
        }
    }

    /// A qubit in \|0⟩: a released one, or a new one added to the state.
    fn allocate(&mut self) -> Result<usize> {
        match &mut self.mode {
            Mode::Simulate => {}
            Mode::Build(recording) => {
                let circuit = &mut recording.circuit;
                let (qubit, needs_reset) =
                    if circuit.qubits < FRESH_QUBITS || recording.free.is_empty() {
                        circuit.qubits += 1;
                        (circuit.qubits - 1, false)
                    } else {
                        recording.free.pop().expect("checked above")
                    };
                if needs_reset || recording.loops > 0 {
                    circuit.instructions.push(Instruction::Reset(qubit));
                }
                return Ok(qubit);
            }
        }
        if let Some(qubit) = self.free.pop() {
            return Ok(qubit);
        }
        self.session()?.add_qubit().map_err(backend_error)
    }

    /// Measures a qubit: its `Bit`, or while recording, the classical bit it was measured into.
    fn measurement(&mut self, qubit: usize) -> Result<Value> {
        if let Mode::Build(recording) = &self.mode {
            let clbit = recording.circuit.clbits;
            self.measure(qubit)?;
            return Ok(Value::Clbit(clbit));
        }
        Ok(Value::Bit(self.measure(qubit)?))
    }

    /// Measures a qubit, consuming it: it is returned to \|0⟩ and freed.
    fn measure(&mut self, qubit: usize) -> Result<bool> {
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
                return Ok(false);
            }
        }
        let session = self.session()?;
        let bit = session.measure(qubit).map_err(backend_error)?;
        if bit {
            // Back to |0⟩ for reuse: the qubit is known to be in |1⟩.
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
        Ok(bit)
    }

    /// Applies a gate with its controls.
    fn execute(&mut self, op: Op) -> Result<()> {
        if op.controls.iter().any(|c| op.targets.contains(c)) {
            return Err(RuntimeError::new(
                "a gate's target is also one of its controls",
            ));
        }
        let wanted = if op.gate == Gate::Swap { 2 } else { 1 };
        if op.targets.len() != wanted {
            return Err(RuntimeError::new("a gate with the wrong number of qubits"));
        }
        match &mut self.mode {
            Mode::Simulate => self.session()?.apply(&op).map_err(backend_error),
            Mode::Build(recording) => {
                recording.circuit.instructions.push(Instruction::Gate(op));
                Ok(())
            }
        }
    }
}

/// The step that sets classical bit `clbit` to a `Bit` or `Bool` value, known or not.
fn assign(clbit: usize, value: &Value) -> Result<Instruction> {
    let (value, _) = value.logic().ok_or_else(|| {
        RuntimeError::new(format!(
            "expected a Bit or Bool to keep in a circuit, found {value:?}"
        ))
    })?;
    Ok(Instruction::Assign { clbit, value })
}

fn backend_error(error: BackendError) -> RuntimeError {
    RuntimeError::new(error.0)
}

// Expressions.

impl Machine<'_> {
    fn rvalue(&self, value: &Rvalue) -> Result<Value> {
        Ok(match value {
            Rvalue::Use(op) => self.operand(op)?,
            Rvalue::Unary(UnOp::Not | UnOp::BitNot, x)
                if let Some(v) = self.operand(x)?.symbolic_logic(None, None) =>
            {
                v
            }
            Rvalue::Unary(op, x) => unary(*op, self.value(x)?)?,
            Rvalue::Binary(op, a, b)
                if let Some(v) = self
                    .operand(a)?
                    .symbolic_logic(Some(*op), Some(&self.operand(b)?)) =>
            {
                v
            }
            Rvalue::Binary(op, a, b) => binary(*op, self.value(a)?, self.value(b)?)?,
            Rvalue::Aggregate(ops) => {
                Value::Aggregate(ops.iter().map(|o| self.operand(o)).collect::<Result<_>>()?)
            }
            Rvalue::Repeat(op, n) => Value::Aggregate(vec![self.operand(op)?; *n as usize]),
            Rvalue::Ref { place, .. } => Value::Ref(self.resolve(place)?),
            // `Bool(b)` and `Bit(flag)` keep a recorded result symbolic.
            Rvalue::Convert(target @ (Ty::BOOL | Ty::BIT), op)
                if let Some(condition) = self.operand(op)?.condition() =>
            {
                Value::Cond(Rc::new(condition), *target == Ty::BIT)
            }
            Rvalue::Convert(target, op) => convert(*target, self.value(op)?)?,
            Rvalue::TryConvert(target, op) => {
                let width = target.width().expect("checked: a sized integer");
                sized::try_from(&self.value(op)?, width)?
            }
            Rvalue::Math(math, ops) => {
                let args = ops
                    .iter()
                    .map(|o| self.operand(o))
                    .collect::<Result<Vec<_>>>()?;
                maths(*math, &args)?
            }
            Rvalue::BigInt(text) => Value::Int(
                Int::parse(text)
                    .ok_or_else(|| RuntimeError::new(format!("`{text}` is not an Int")))?,
            ),
            Rvalue::FloatConst(c) => Value::Float(match c {
                FloatConst::Pi => std::f64::consts::PI,
                FloatConst::Tau => std::f64::consts::TAU,
                FloatConst::E => std::f64::consts::E,
            }),
            Rvalue::Pauli(p, index) => {
                let index = self.qubit_index(index)?;
                let pauli = match p {
                    IrPauli::X => Pauli::X,
                    IrPauli::Y => Pauli::Y,
                    IrPauli::Z => Pauli::Z,
                };
                Value::Observable(Observable::pauli(pauli, index))
            }
            Rvalue::Identity => Value::Observable(Observable::identity()),
            Rvalue::Variant(index, ops) => Value::Variant(
                *index,
                ops.iter().map(|o| self.operand(o)).collect::<Result<_>>()?,
            ),
            Rvalue::Tag(op) => match self.value(op)? {
                Value::Variant(index, _) => Value::Int(Int::from(i64::from(index))),
                other => {
                    return Err(RuntimeError::new(format!(
                        "expected an enum value, found {other:?}"
                    )));
                }
            },
            Rvalue::Dist(method, ops, elem) => {
                let Value::Distribution(dist) = self.operand(&ops[0])? else {
                    return Err(RuntimeError::new("expected a distribution"));
                };
                if dist.not_run && *method != DistMethod::Shots {
                    return Err(RuntimeError::unknown(
                        "the program uses the results of a `sample`",
                    ));
                }
                let key = |m: &Self, i: usize| -> Result<String> {
                    Ok(format::display(m.program, &m.value(&ops[i])?, *elem))
                };
                match method {
                    DistMethod::Shots => Value::Int(Int::from(dist.shots as i64)),
                    DistMethod::Count => Value::Int(Int::from(dist.count(&key(self, 1)?) as i64)),
                    DistMethod::Probability => {
                        Value::Float(dist.count(&key(self, 1)?) as f64 / dist.shots as f64)
                    }
                    DistMethod::MostLikely => dist.outcomes[0].value.clone(),
                }
            }
            Rvalue::Format(parts) => {
                let mut text = String::new();
                for part in parts {
                    match part {
                        FormatPart::Text(t) => text.push_str(t),
                        FormatPart::Value(op, ty, spec) => {
                            text.push_str(&format::format(
                                self.program,
                                &self.value(op)?,
                                *ty,
                                spec.as_ref(),
                            ));
                        }
                    }
                }
                Value::Str(text.into())
            }
            Rvalue::Projector {
                x_basis,
                index,
                bit,
            } => {
                let index = self.qubit_index(index)?;
                let bit = self.truth(bit)?;
                Value::Observable(Observable::projector(*x_basis, index, bit))
            }
        })
    }

    fn qubit_index(&self, op: &Operand) -> Result<usize> {
        let index = self.int(op)?;
        usize::try_from(index)
            .map_err(|_| RuntimeError::new(format!("qubit index {index} is negative")))
    }
}

fn unary(op: UnOp, x: Value) -> Result<Value> {
    Ok(match (op, x) {
        (UnOp::Neg, Value::Int(v)) => Value::Int(v.neg()),
        (UnOp::Neg, Value::Float(v)) => Value::Float(-v),
        (UnOp::Neg, Value::F32(v)) => Value::F32(-v),
        (UnOp::Neg, Value::Sized(v, w)) => sized::neg(v, w)?,
        (UnOp::BitNot, Value::Sized(v, w)) => sized::not(v, w),
        (UnOp::Neg, Value::Observable(o)) => Value::Observable(o.scale(-1.0)),
        (UnOp::BitNot, Value::Int(v)) => Value::Int(v.not()),
        (UnOp::BitNot, Value::Bit(v)) => Value::Bit(!v),
        (UnOp::Not, Value::Bool(v)) => Value::Bool(!v),
        (op, x) => return Err(RuntimeError::new(format!("cannot apply {op:?} to {x:?}"))),
    })
}

fn binary(op: BinOp, a: Value, b: Value) -> Result<Value> {
    use BinOp::*;
    Ok(match (a, b) {
        (Value::Int(a), Value::Int(b)) => match op {
            Add => Value::Int(a.add(&b)),
            Sub => Value::Int(a.sub(&b)),
            Mul => Value::Int(a.mul(&b)),
            Div => Value::Int(
                a.div(&b)
                    .ok_or_else(|| RuntimeError::new("division by zero"))?,
            ),
            Rem => Value::Int(
                a.rem(&b)
                    .ok_or_else(|| RuntimeError::new("remainder by zero"))?,
            ),
            BitAnd => Value::Int(a.and(&b)),
            BitOr => Value::Int(a.or(&b)),
            BitXor => Value::Int(a.xor(&b)),
            Shl | Shr => {
                let shift = b
                    .to_i64()
                    .and_then(|s| u32::try_from(s).ok())
                    .ok_or_else(|| RuntimeError::new(format!("cannot shift by {b}")))?;
                Value::Int(if op == Shl {
                    a.shl(shift)
                } else {
                    a.shr(shift)
                })
            }
            Eq => Value::Bool(a == b),
            NotEq => Value::Bool(a != b),
            Lt => Value::Bool(a < b),
            Le => Value::Bool(a <= b),
            Gt => Value::Bool(a > b),
            Ge => Value::Bool(a >= b),
            And | Or => return Err(RuntimeError::new("logic on Int")),
        },
        (Value::Sized(a, w), b) => sized::binary(op, a, w, &b)?,
        (Value::F32(a), Value::F32(b)) => match op {
            Add => Value::F32(a + b),
            Sub => Value::F32(a - b),
            Mul => Value::F32(a * b),
            Div => Value::F32(a / b),
            Rem => Value::F32(a % b),
            Eq => Value::Bool(a == b),
            NotEq => Value::Bool(a != b),
            Lt => Value::Bool(a < b),
            Le => Value::Bool(a <= b),
            Gt => Value::Bool(a > b),
            Ge => Value::Bool(a >= b),
            _ => return Err(RuntimeError::new(format!("{} on f32", op.as_str()))),
        },
        (Value::Float(a), Value::Float(b)) => match op {
            Add => Value::Float(a + b),
            Sub => Value::Float(a - b),
            Mul => Value::Float(a * b),
            Div => Value::Float(a / b),
            Rem => Value::Float(a % b),
            Eq => Value::Bool(a == b),
            NotEq => Value::Bool(a != b),
            Lt => Value::Bool(a < b),
            Le => Value::Bool(a <= b),
            Gt => Value::Bool(a > b),
            Ge => Value::Bool(a >= b),
            _ => return Err(RuntimeError::new(format!("{} on Float", op.as_str()))),
        },
        (Value::Bool(a), Value::Bool(b)) => match op {
            And => Value::Bool(a && b),
            Or => Value::Bool(a || b),
            Eq => Value::Bool(a == b),
            NotEq => Value::Bool(a != b),
            _ => return Err(RuntimeError::new(format!("{} on Bool", op.as_str()))),
        },
        (a @ Value::Variant(..), b @ Value::Variant(..)) => match op {
            Eq => Value::Bool(a == b),
            NotEq => Value::Bool(a != b),
            _ => return Err(RuntimeError::new(format!("{} on an enum", op.as_str()))),
        },
        (Value::Str(a), Value::Str(b)) => match op {
            Eq => Value::Bool(a == b),
            NotEq => Value::Bool(a != b),
            _ => return Err(RuntimeError::new(format!("{} on String", op.as_str()))),
        },
        (Value::Bit(a), Value::Bit(b)) => match op {
            BitAnd => Value::Bit(a & b),
            BitOr => Value::Bit(a | b),
            BitXor => Value::Bit(a ^ b),
            Eq => Value::Bool(a == b),
            NotEq => Value::Bool(a != b),
            _ => return Err(RuntimeError::new(format!("{} on Bit", op.as_str()))),
        },
        (Value::Observable(a), Value::Observable(b)) => Value::Observable(match op {
            Add => a.add(&b),
            Sub => a.add(&b.scale(-1.0)),
            Mul => a.mul(&b).map_err(RuntimeError::new)?,
            _ => return Err(RuntimeError::new(format!("{} on Observable", op.as_str()))),
        }),
        (Value::Float(c), Value::Observable(o)) | (Value::Observable(o), Value::Float(c))
            if op == Mul =>
        {
            Value::Observable(o.scale(c))
        }
        (a, b) => {
            return Err(RuntimeError::new(format!(
                "cannot apply `{}` to {a:?} and {b:?}",
                op.as_str()
            )));
        }
    })
}

/// A conversion the checker allowed (`spec/types.md` §4).
fn convert(target: Ty, value: Value) -> Result<Value> {
    let float = |value: &Value| match *value {
        Value::Int(ref v) => Some(v.to_f64()),
        Value::Sized(v, _) => Some(v as f64),
        Value::Float(v) => Some(v),
        Value::F32(v) => Some(f64::from(v)),
        _ => None,
    };
    Ok(match (target, value) {
        // `f64` values are `Float`s.
        (Ty::FLOAT | Ty::F64, value) if let Some(x) = float(&value) => Value::Float(x),
        // Rounded to nearest, from a value already a float or an exact one otherwise.
        (Ty::F32, Value::Sized(v, _)) => Value::F32(v as f32),
        (Ty::F32, value) if let Some(x) = float(&value) => Value::F32(x as f32),
        (Ty::INT, Value::Bit(v) | Value::Bool(v)) => Value::Int(Int::from(i64::from(v))),
        (Ty::INT, Value::Sized(v, _)) => Value::Int(sized::to_int(v)),
        (target, Value::Sized(v, _)) if let Some(w) = target.width() => Value::Sized(v, w),
        (target, Value::Bit(v) | Value::Bool(v)) if let Some(w) = target.width() => {
            Value::Sized(i128::from(v), w)
        }
        (target, Value::Aggregate(bits)) if let Some(w) = target.width() => {
            sized::from_bits(&bits, w)?
        }
        (Ty::BIT, Value::Bool(v)) => Value::Bit(v),
        (Ty::BOOL, Value::Bit(v)) => Value::Bool(v),
        (_, value) => value,
    })
}

fn maths(math: Math, args: &[Value]) -> Result<Value> {
    let float = |i: usize| match args.get(i) {
        Some(Value::Float(v)) => Ok(*v),
        other => Err(RuntimeError::new(format!(
            "expected a Float, found {other:?}"
        ))),
    };
    // Exact, as spec/types.md §4 requires; NaN and infinities have no integer.
    let to_int = |x: f64| -> Result<Value> {
        Int::from_f64(x)
            .map(Value::Int)
            .ok_or_else(|| RuntimeError::new(format!("cannot convert {x} to an Int")))
    };
    // `f32` maths is `Float` maths, rounded back.
    if let Some(Value::F32(_)) = args.first() {
        let widened: Vec<Value> = args
            .iter()
            .map(|a| match *a {
                Value::F32(v) => Value::Float(f64::from(v)),
                ref other => other.clone(),
            })
            .collect();
        return Ok(match maths(math, &widened)? {
            Value::Float(v) => Value::F32(v as f32),
            other => other,
        });
    }
    if let Some(&Value::Sized(a, w)) = args.first() {
        let other = || match args.get(1) {
            Some(&Value::Sized(b, _)) => Ok(b),
            other => Err(RuntimeError::new(format!(
                "expected a `{}`, found {other:?}",
                w.name()
            ))),
        };
        return Ok(match math {
            Math::Abs if a < 0 => sized::neg(a, w)?,
            Math::Abs => Value::Sized(a, w),
            Math::Min => Value::Sized(a.min(other()?), w),
            Math::Max => Value::Sized(a.max(other()?), w),
            Math::Bits => sized::bits(a, w),
            _ => return Err(RuntimeError::new(format!("{math:?} on `{}`", w.name()))),
        });
    }
    if let Some(Value::Int(a)) = args.first() {
        return Ok(Value::Int(match (math, args.get(1)) {
            (Math::Abs, _) => a.abs(),
            (Math::Min, Some(Value::Int(b))) => a.min(b).clone(),
            (Math::Max, Some(Value::Int(b))) => a.max(b).clone(),
            _ => return Err(RuntimeError::new(format!("{math:?} on Int"))),
        }));
    }
    let x = float(0)?;
    Ok(match math {
        Math::Sin => Value::Float(x.sin()),
        Math::Cos => Value::Float(x.cos()),
        Math::Tan => Value::Float(x.tan()),
        Math::Asin => Value::Float(x.asin()),
        Math::Acos => Value::Float(x.acos()),
        Math::Atan => Value::Float(x.atan()),
        Math::Atan2 => Value::Float(x.atan2(float(1)?)),
        Math::Sqrt => Value::Float(x.sqrt()),
        Math::Exp => Value::Float(x.exp()),
        Math::Ln => Value::Float(x.ln()),
        Math::Abs => Value::Float(x.abs()),
        Math::Min => Value::Float(x.min(float(1)?)),
        Math::Max => Value::Float(x.max(float(1)?)),
        Math::Trunc => to_int(x.trunc())?,
        Math::Round => to_int(x.round())?,
        Math::Floor => to_int(x.floor())?,
        Math::Ceil => to_int(x.ceil())?,
        Math::Bits => return Err(RuntimeError::new("`bits` on a Float")),
    })
}

// Expectation values.

/// `<prep| observable |prep> within precision` (`spec/types.md` §3). On a backend that sees
/// the state, exact when the preparation measures nothing, and otherwise the average of the
/// exact value over enough runs for `precision`. On any other backend, estimated by measuring.
fn expectation(
    program: &Program,
    backend: &dyn Backend,
    prep: &Prep,
    args: Vec<Value>,
    observable: &Observable,
    precision: f64,
    seed: u64,
) -> Result<f64> {
    let mut seeds = SmallRng::seed_from_u64(seed);
    if !backend.exact_expectations() {
        return estimate(
            program, backend, prep, &args, observable, precision, &mut seeds,
        );
    }
    let mut machine = Machine::new(program, backend, seeds.random());
    let qubits = machine.prepare(prep, args.clone())?;
    let first = machine.exact_expectation(observable, &qubits)?;
    if !machine.measured {
        return Ok(first);
    }
    // Each run's exact value lies within ± the sum of the coefficients of non-identity terms.
    let weight: f64 = observable
        .terms
        .iter()
        .filter(|(_, p)| !p.is_empty())
        .map(|(c, _)| c.abs())
        .sum();
    let runs = runs_for(weight, weight, precision)?;
    let mut total = first;
    for _ in 1..runs {
        let mut machine = Machine::new(program, backend, seeds.random());
        let qubits = machine.prepare(prep, args.clone())?;
        total += machine.exact_expectation(observable, &qubits)?;
    }
    Ok(total / runs as f64)
}

/// A set of Pauli terms that can be measured together: each qubit is measured in one basis.
struct TermGroup<'o> {
    /// `(index in the preparation, basis)`.
    basis: Vec<(usize, Pauli)>,
    /// `(coefficient, product)`.
    terms: Vec<&'o (f64, Vec<(usize, Pauli)>)>,
}

/// Estimates an expectation value by measuring: terms that agree on each qubit's basis are
/// grouped, and each group is measured over enough runs of the preparation for `precision`, as one
/// circuit when the preparation is one, otherwise run by run. A term's value is the average of
/// ±1, the parity of its qubits' outcomes.
fn estimate(
    program: &Program,
    backend: &dyn Backend,
    prep: &Prep,
    args: &[Value],
    observable: &Observable,
    precision: f64,
    seeds: &mut SmallRng,
) -> Result<f64> {
    let mut total = 0.0;
    let mut groups: Vec<TermGroup> = Vec::new();
    for term in &observable.terms {
        if term.1.is_empty() {
            total += term.0;
            continue;
        }
        let fits = |group: &TermGroup| {
            term.1
                .iter()
                .all(|&(q, p)| group.basis.iter().all(|&(g, b)| g != q || b == p))
        };
        match groups.iter_mut().find(|g| fits(g)) {
            Some(group) => {
                for &(q, p) in &term.1 {
                    if !group.basis.iter().any(|&(g, _)| g == q) {
                        group.basis.push((q, p));
                    }
                }
                group.terms.push(term);
            }
            None => groups.push(TermGroup {
                basis: term.1.clone(),
                terms: vec![term],
            }),
        }
    }
    let is_circuit = match prep {
        Prep::Ket(_) => true,
        Prep::Call(f, _) => program.functions[f.index()].sampled,
    };
    let part = |group: &TermGroup| group.terms.iter().map(|(c, _)| c.abs()).sum::<f64>();
    let weight: f64 = groups.iter().map(part).sum();
    for group in &groups {
        let shots = runs_for(part(group), weight, precision)?;
        // Outcomes with bit `k` for `group.basis[k]`.
        let mut outcomes = None;
        if is_circuit {
            outcomes =
                measure_as_circuit(program, backend, prep, args, &group.basis, shots, seeds)?;
        }
        let outcomes = match outcomes {
            Some(outcomes) => outcomes,
            None if !backend.executes() => {
                return Err(RuntimeError::unknown(
                    "the program uses an expectation value whose preparation is not a circuit",
                ));
            }
            None => {
                let mut counts: std::collections::BTreeMap<u64, usize> = Default::default();
                for _ in 0..shots {
                    let mut machine = Machine::new(program, backend, seeds.random());
                    let qubits = machine.prepare(prep, args.to_vec())?;
                    let mut bits = 0u64;
                    for (k, value) in machine
                        .measure_basis(&qubits, &group.basis)?
                        .iter()
                        .enumerate()
                    {
                        if *value == Value::Bit(true) {
                            bits |= 1 << k;
                        }
                    }
                    *counts.entry(bits).or_default() += 1;
                }
                counts.into_iter().collect()
            }
        };
        let runs: usize = outcomes.iter().map(|&(_, c)| c).sum();
        for &&(coeff, ref product) in &group.terms {
            let mask = product.iter().fold(0u64, |mask, &(q, _)| {
                let k = group
                    .basis
                    .iter()
                    .position(|&(g, _)| g == q)
                    .expect("in the group");
                mask | 1 << k
            });
            let sum: i64 = outcomes
                .iter()
                .map(|&(bits, count)| {
                    let sign = if (bits & mask).count_ones() % 2 == 0 {
                        1
                    } else {
                        -1
                    };
                    sign * count as i64
                })
                .sum();
            if runs > 0 {
                total += coeff * sum as f64 / runs as f64;
            }
        }
    }
    if !backend.executes() {
        return Err(RuntimeError::unknown(
            "the program uses an expectation value",
        ));
    }
    Ok(total)
}

/// Records the preparation followed by measurements in `basis` as one circuit and runs it.
/// `None` if it cannot be recorded or the backend does not take it, so it runs run by run.
fn measure_as_circuit(
    program: &Program,
    backend: &dyn Backend,
    prep: &Prep,
    args: &[Value],
    basis: &[(usize, Pauli)],
    shots: usize,
    seeds: &mut SmallRng,
) -> Result<Option<Vec<(u64, usize)>>> {
    let mut builder =
        Machine::new(program, backend, seeds.random()).with_mode(Mode::Build(Recording::default()));
    let Ok(qubits) = builder.prepare(prep, args.to_vec()) else {
        return Ok(None);
    };
    let clbits: Vec<usize> = builder
        .measure_basis(&qubits, basis)?
        .iter()
        .map(|v| match v {
            Value::Clbit(k) => *k,
            _ => unreachable!("measurements are recorded"),
        })
        .collect();
    if builder.needs_values.get() {
        return Ok(None);
    }
    let Mode::Build(recording) = std::mem::take(&mut builder.mode) else {
        unreachable!("the builder records")
    };
    if recording.circuit.clbits > 64 {
        return Ok(None);
    }
    let Some(counts) = backend.run_circuit(&recording.circuit, shots, seeds.random()) else {
        return Ok(None);
    };
    let counts = counts.map_err(backend_error)?;
    let outcomes = counts
        .into_iter()
        .map(|(bits, count)| {
            let picked = clbits
                .iter()
                .enumerate()
                .fold(0u64, |acc, (k, &c)| acc | ((bits >> c) & 1) << k);
            (picked, count)
        })
        .collect();
    Ok(Some(outcomes))
}

impl Machine<'_> {
    /// Measures the preparation's qubits `basis` names, each in its basis, consuming them:
    /// `Bit`s, or while recording, the classical bits they go into.
    fn measure_basis(&mut self, qubits: &[usize], basis: &[(usize, Pauli)]) -> Result<Vec<Value>> {
        let mut values = Vec::with_capacity(basis.len());
        for &(index, pauli) in basis {
            let Some(&qubit) = qubits.get(index) else {
                return Err(RuntimeError::new(format!(
                    "the observable acts on qubit {index}, but the preparation has {} qubit{}",
                    qubits.len(),
                    if qubits.len() == 1 { "" } else { "s" }
                )));
            };
            // Into the Z basis: X by H, Y by S† then H.
            let gates: &[(Gate, bool)] = match pauli {
                Pauli::X => &[(Gate::H, false)],
                Pauli::Y => &[(Gate::S, true), (Gate::H, false)],
                Pauli::Z => &[],
            };
            for &(gate, adjoint) in gates {
                self.execute(Op {
                    gate,
                    adjoint,
                    params: Vec::new(),
                    controls: Vec::new(),
                    targets: vec![qubit],
                })?;
            }
            values.push(self.measurement(qubit)?);
        }
        Ok(values)
    }

    /// Runs a preparation, returning its qubits in order.
    fn prepare(&mut self, prep: &Prep, args: Vec<Value>) -> Result<Vec<usize>> {
        let value = match prep {
            Prep::Ket(state) => {
                let mut qubits = Vec::new();
                for label in noev_syntax::state::labels(state) {
                    use noev_syntax::state::Label;
                    let qubit = self.allocate()?;
                    let gates: &[Gate] = match label {
                        Label::Zero => &[],
                        Label::One => &[Gate::X],
                        Label::Plus => &[Gate::H],
                        Label::Minus => &[Gate::X, Gate::H],
                        Label::PlusI => &[Gate::H, Gate::S],
                        Label::MinusI => &[Gate::X, Gate::H, Gate::S],
                    };
                    for &gate in gates {
                        self.execute(Op {
                            gate,
                            adjoint: false,
                            params: Vec::new(),
                            controls: Vec::new(),
                            targets: vec![qubit],
                        })?;
                    }
                    qubits.push(Value::Qubit(qubit));
                }
                Value::Aggregate(qubits)
            }
            Prep::Call(func, _) => self.call(*func, args)?,
        };
        let mut qubits = Vec::new();
        value.qubits(&mut qubits);
        Ok(qubits)
    }

    fn exact_expectation(&self, observable: &Observable, qubits: &[usize]) -> Result<f64> {
        if observable.width() > qubits.len() {
            return Err(RuntimeError::new(format!(
                "the observable acts on qubit {}, but the preparation has {} qubit{}",
                observable.width() - 1,
                qubits.len(),
                if qubits.len() == 1 { "" } else { "s" }
            )));
        }
        let terms: Vec<_> = observable
            .terms
            .iter()
            .map(|(coeff, product)| {
                (
                    *coeff,
                    product.iter().map(|&(i, p)| (qubits[i], p)).collect(),
                )
            })
            .collect();
        let name = self.backend.name();
        let session = self.session.as_ref().ok_or_else(|| {
            RuntimeError::new(format!("`{name}` cannot compute expectation values yet"))
        })?;
        match session.expectation(&terms) {
            Some(value) => value.map_err(backend_error),
            None => Err(RuntimeError::new(format!(
                "`{name}` cannot compute expectation values yet"
            ))),
        }
    }
}
