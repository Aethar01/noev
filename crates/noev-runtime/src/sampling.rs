//! `sample`: repeating a closed run and counting its results (`spec/execution.md` §3).

use std::collections::BTreeMap;

use noev_ir::ir::{FuncId, Program};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use crate::backend::Backend;
use crate::machine::{Machine, Mode, Recording, RuntimeError};
use crate::value::{Distribution, Outcome, Value};

/// Runs `func(args)` `shots` times and counts the results, with returned qubits measured at
/// the end of each run. `key` gives each result's printed form, which identifies equal
/// results. The same `seed` gives the same distribution.
///
/// A run that is a circuit (`spec/execution.md` §4) is recorded as a circuit once, which the
/// backend runs for all shots, if it accepts circuits; each distinct outcome then has its
/// classical code replayed to get the result. Other runs go to the backend gate by gate, a
/// shot at a time, except that one returning qubits without measuring anything is run once
/// and its state sampled, on a simulator.
pub fn sample(
    program: &Program,
    backend: &dyn Backend,
    func: FuncId,
    args: Vec<Value>,
    shots: usize,
    seed: u64,
    key: impl Fn(&Value) -> String,
) -> Result<Distribution, RuntimeError> {
    if shots == 0 {
        return Err(RuntimeError::new("`sample` needs at least one shot"));
    }
    let mut seeds = SmallRng::seed_from_u64(seed);
    let mut counts: BTreeMap<String, Outcome> = BTreeMap::new();
    let mut add = |value: Value, count: usize| {
        let k = key(&value);
        counts
            .entry(k.clone())
            .or_insert_with(|| Outcome {
                key: k,
                value,
                count: 0,
            })
            .count += count;
    };

    if program.functions[func.index()].sampled
        && let Some(outcomes) =
            sample_circuit(program, backend, func, &args, shots, seeds.random())?
    {
        for (value, count) in outcomes {
            add(value, count);
        }
        let mut dist = Distribution::new(counts.into_values().collect(), shots);
        dist.from_circuit = true;
        dist.not_run = !backend.executes();
        return Ok(dist);
    }
    // A run that goes shot by shot has no circuit to record.
    if !backend.executes() {
        let mut dist = Distribution::new(Vec::new(), shots);
        dist.not_run = true;
        return Ok(dist);
    }

    let mut first = Machine::new(program, backend, seeds.random());
    let value = first.call(func, args.clone())?;
    let mut qubits = Vec::new();
    value.qubits(&mut qubits);
    let sampled = (!qubits.is_empty() && !first.measured())
        .then(|| first.sample(&qubits, shots))
        .flatten();
    if let Some(counts) = sampled {
        for (bits, count) in counts {
            add(value.with_bits(bits), count);
        }
    } else {
        add(first.measure_value(value)?, 1);
        for _ in 1..shots {
            let mut machine = Machine::new(program, backend, seeds.random());
            let value = machine.call(func, args.clone())?;
            add(machine.measure_value(value)?, 1);
        }
    }
    Ok(Distribution::new(counts.into_values().collect(), shots))
}

/// The circuit path: record the run as a circuit, run it on the backend, and replay each
/// distinct outcome. `None` if the run cannot be recorded (it failed while recording, or needs
/// more classical bits than an outcome can hold) or the backend does not take the circuit, in
/// which case it runs shot by shot.
fn sample_circuit(
    program: &Program,
    backend: &dyn Backend,
    func: FuncId,
    args: &[Value],
    shots: usize,
    seed: u64,
) -> Result<Option<Vec<(Value, usize)>>, RuntimeError> {
    let mut seeds = SmallRng::seed_from_u64(seed);
    let mut builder =
        Machine::new(program, backend, seeds.random()).with_mode(Mode::Build(Recording::default()));
    let Ok(value) = builder.call(func, args.to_vec()) else {
        return Ok(None);
    };
    let Ok(template) = builder.measure_value(value) else {
        return Ok(None);
    };
    // If nothing needed a measurement result's value, the result is the template with each
    // outcome's bits filled in; otherwise each outcome's classical code is replayed.
    let replay = builder.needs_values.get();
    let Mode::Build(recording) = std::mem::take(&mut builder.mode) else {
        unreachable!("the builder records")
    };
    let circuit = recording.circuit;
    if circuit.clbits > 64 {
        return Ok(None);
    }
    let Some(counts) = backend.run_circuit(&circuit, shots, seeds.random()) else {
        return Ok(None);
    };
    let counts = counts.map_err(|e| RuntimeError::new(e.0))?;

    let mut outcomes = Vec::with_capacity(counts.len());
    if !replay {
        for (bits, count) in counts {
            outcomes.push((template.with_clbits(bits), count));
        }
        return Ok(Some(outcomes));
    }
    for (bits, count) in counts {
        let mode = Mode::Replay {
            bits,
            next: 0,
            qubits: 0,
        };
        let mut replay = Machine::new(program, backend, seeds.random()).with_mode(mode);
        let value = replay.call(func, args.to_vec())?;
        outcomes.push((replay.measure_value(value)?, count));
    }
    Ok(Some(outcomes))
}
