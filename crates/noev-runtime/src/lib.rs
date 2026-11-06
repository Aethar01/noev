//! Executes Noev programs, sending their quantum operations to a backend.

pub mod backend;
pub mod format;
pub mod machine;
pub mod qasm;
pub mod sampling;
pub mod value;

use noev_ir::ir::{FuncId, Program};
use std::cell::RefCell;
use std::rc::Rc;

pub use backend::Backend;
pub use backend::ketrs::Ketrs;
pub use machine::{Machine, Output, RuntimeError};
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
    let mut machine = Machine::new(program, backend, seed).with_output(output.clone());
    let result = machine.call(main, Vec::new()).map(|_| ());
    let mut output = output.borrow_mut();
    let flushed = output.stdout.flush().and_then(|()| output.stderr.flush());
    result?;
    flushed.map_err(|e| RuntimeError::new(format!("cannot write output: {e}")))
}

/// How often each result of a program occurred.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Counts {
    /// `(result, count)`, most frequent first, ties in result order.
    pub results: Vec<(String, usize)>,
    /// How many shots ran.
    pub shots: usize,
}

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
