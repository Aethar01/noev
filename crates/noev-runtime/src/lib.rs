//! Executes Noev programs on the ketrs simulator.

pub mod format;
pub mod machine;
pub mod value;

use noev_ir::ir::{FuncId, Program};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

pub use machine::{Machine, Output, RuntimeError};
pub use value::Value;

/// Runs `main` once, as `noev run` does, writing its output to `output`. The same `seed`
/// gives the same run.
pub fn run_main(program: &Program, seed: u64, output: Output) -> Result<(), RuntimeError> {
    let main = program.main.ok_or_else(|| RuntimeError {
        message: "the program has no `fn main()`".into(),
        span: None,
    })?;
    let output = Rc::new(RefCell::new(output));
    let mut machine = Machine::new(program, seed).with_output(output.clone());
    let result = machine.call(main, Vec::new()).map(|_| ());
    let mut output = output.borrow_mut();
    let flushed = output.stdout.flush().and_then(|()| output.stderr.flush());
    result?;
    flushed.map_err(|e| RuntimeError {
        message: format!("cannot write output: {e}"),
        span: None,
    })
}

/// How often each result of a program occurred.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Counts {
    /// `(result, count)`, most frequent first, ties in result order.
    pub results: Vec<(String, usize)>,
    /// How many shots ran.
    pub shots: usize,
}

/// Runs `entry` `shots` times and counts its results. The same `seed` gives the same counts.
///
/// If `entry` returns qubits and measures nothing itself, every shot would produce the same
/// state, so it runs once and the simulator samples the returned qubits `shots` times
/// (`spec/core.md` §6). Otherwise each shot runs on a fresh simulator, and any returned
/// qubits are measured at its end.
pub fn run(
    program: &Program,
    entry: FuncId,
    shots: usize,
    seed: u64,
) -> Result<Counts, RuntimeError> {
    let mut seeds = SmallRng::seed_from_u64(seed);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    let mut first = Machine::new(program, seeds.random());
    let value = first.call(entry, Vec::new())?;
    let mut qubits = Vec::new();
    value.qubits(&mut qubits);
    if !qubits.is_empty() && !first.measured() {
        for (bits, count) in first.sample(&qubits, shots) {
            *counts.entry(value.with_bits(bits).to_string()).or_default() += count;
        }
    } else {
        let value = first.measure_value(value)?;
        *counts.entry(value.to_string()).or_default() += 1;
        for _ in 1..shots {
            let mut machine = Machine::new(program, seeds.random());
            let value = machine.call(entry, Vec::new())?;
            let value = machine.measure_value(value)?;
            *counts.entry(value.to_string()).or_default() += 1;
        }
    }

    let mut results: Vec<(String, usize)> = counts.into_iter().collect();
    results.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(Counts { results, shots })
}
