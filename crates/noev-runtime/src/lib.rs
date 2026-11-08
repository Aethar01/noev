//! Executes Noev programs, sending their quantum operations to a backend.

pub mod backend;
pub mod format;
pub mod fourier;
pub mod int;
pub mod kak;
pub mod machine;
pub mod qasm;
pub mod sampling;
pub mod sized;
pub mod unitary;
pub mod value;

use noev_ir::ir::{FuncId, Program};
use std::cell::RefCell;
use std::rc::Rc;

pub use backend::Backend;
#[cfg(feature = "ketrs")]
pub use backend::ketrs::Ketrs;
pub use machine::{Machine, Output, RuntimeError};
use machine::{Mode, Recording};
pub use value::Value;

/// Runs `main` once on `backend`, as `noev run` does, writing its output to `output`. The
/// same `seed` gives the same run.
pub fn run_main(
    program: &Program,
    backend: &dyn Backend,
    seed: u64,
    output: Output,
) -> Result<(), RuntimeError> {
    let main = program
        .main
        .ok_or_else(|| RuntimeError::new("the program has no `fn main()`"))?;
    let output = Rc::new(RefCell::new(output));
    // `main` is a run with one shot like any other: on a backend that only takes circuits, it
    // goes as a circuit when it is one (spec/runtime.md §3).
    let result = if backend.session(seed).is_none() && program.functions[main.index()].sampled {
        main_as_circuit(program, backend, main, seed, output.clone())
    } else {
        let mut machine = Machine::new(program, backend, seed).with_output(output.clone());
        machine.call(main, Vec::new()).map(|_| ())
    };
    let mut output = output.borrow_mut();
    let flushed = output.stdout.flush().and_then(|()| output.stderr.flush());
    result?;
    flushed.map_err(|e| RuntimeError::new(format!("cannot write output: {e}")))
}

/// Runs `main` as a one-shot circuit: recorded, run on the backend, then its classical code
/// (what it prints, too) run again for the outcome. The repeated runs it starts are computed
/// while recording and reused, so each runs once.
fn main_as_circuit(
    program: &Program,
    backend: &dyn Backend,
    main: FuncId,
    seed: u64,
    output: Rc<RefCell<Output>>,
) -> Result<(), RuntimeError> {
    let mut recording = Recording::default();
    recording.runs = Some(Default::default());
    let mut builder = Machine::new(program, backend, seed).with_mode(Mode::Build(recording));
    builder.call(main, Vec::new())?;
    let Mode::Build(recording) = std::mem::take(&mut builder.mode) else {
        unreachable!("the builder records")
    };
    let counts = backend
        .run_circuit(&recording.circuit, 1, seed)
        .ok_or_else(|| {
            RuntimeError::new(format!(
                "`{}` does not take `main`'s circuit",
                backend.name()
            ))
        })?
        .map_err(|e| RuntimeError::new(e.0))?;
    if !backend.executes() {
        return Ok(());
    }
    let bits = counts
        .first()
        .map(|&(bits, _)| bits)
        .ok_or_else(|| RuntimeError::new("the backend gave no outcome for `main`"))?;
    let mut replay = Recording::knowing(bits);
    replay.runs = recording.runs;
    let mut machine = Machine::new(program, backend, seed)
        .with_mode(Mode::Build(replay))
        .with_output(output);
    machine.call(main, Vec::new()).map(|_| ())
}

/// How often each result of a program occurred.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Counts {
    /// `(result, count)`, most frequent first, ties in result order.
    pub results: Vec<(String, usize)>,
    /// How many shots ran.
    pub shots: usize,
}

#[cfg(feature = "ketrs")]
/// Runs `entry` `shots` times on ketrs and counts its results, keyed by how each result prints
/// without type information. A convenience for tests; programs use `sample`.
pub fn run(
    program: &Program,
    entry: FuncId,
    shots: usize,
    seed: u64,
) -> Result<Counts, RuntimeError> {
    let dist = sampling::sample(program, &Ketrs, entry, Vec::new(), shots, seed, |v| {
        v.to_string()
    })?;
    Ok(Counts {
        results: dist
            .outcomes
            .iter()
            .map(|o| (o.key.clone(), o.count))
            .collect(),
        shots,
    })
}
