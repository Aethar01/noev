//! The ketrs statevector simulator, at both levels.

use std::collections::BTreeMap;

use ketrs::{Operation, PauliString, PauliSum, Statevector};
use noev_sema::prelude::Gate;
use rand::SeedableRng;
use rand::rngs::SmallRng;

use super::{Backend, BackendError, Circuit, Condition, Counts, Instruction, Op, Pauli, Session};

/// The most qubits a statevector holds: 2^30 amplitudes take 16 GiB.
const MAX_QUBITS: usize = 30;

/// The ketrs simulator.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ketrs;

impl Backend for Ketrs {
    fn name(&self) -> &str {
        "ketrs"
    }

    fn exact_expectations(&self) -> bool {
        true
    }

    fn session(&self, seed: u64) -> Option<Box<dyn Session>> {
        Some(Box::new(KetrsSession {
            state: Statevector::zero(0),
            rng: SmallRng::seed_from_u64(seed),
        }))
    }

    fn run_circuit(
        &self,
        circuit: &Circuit,
        shots: usize,
        seed: u64,
    ) -> Option<Result<Counts, BackendError>> {
        if circuit.qubits > MAX_QUBITS || circuit.clbits > 64 {
            return None;
        }
        let converted = match convert(circuit, &circuit.instructions) {
            Ok(converted) => converted,
            Err(e) => return Some(Err(e)),
        };
        let mut rng = SmallRng::seed_from_u64(seed);
        Some(
            Statevector::zero(circuit.qubits)
                .sample_circuit(&converted, shots, &mut rng)
                .map(|counts| counts.into_iter().collect())
                .map_err(sim_error),
        )
    }
}

/// The ketrs circuit for `instructions`, on `circuit`'s qubits and classical bits.
fn convert(
    circuit: &Circuit,
    instructions: &[Instruction],
) -> Result<ketrs::Circuit, BackendError> {
    let mut converted = ketrs::Circuit::with_clbits(circuit.qubits, circuit.clbits);
    for instruction in instructions {
        match instruction {
            Instruction::Gate(op) => {
                converted.push(operation(op)?).map_err(sim_error)?;
            }
            &Instruction::Measure { qubit, clbit } => {
                converted.measure(qubit, clbit);
            }
            &Instruction::Reset(qubit) => {
                converted.reset(qubit);
            }
            Instruction::If {
                condition,
                then,
                otherwise,
            } => {
                let instruction = ketrs::Instruction::IfElse {
                    condition: expr(condition),
                    then: Box::new(convert(circuit, then)?),
                    otherwise: Box::new(convert(circuit, otherwise)?),
                };
                converted.push_instruction(instruction).map_err(sim_error)?;
            }
            Instruction::While { condition, body } => {
                let instruction = ketrs::Instruction::While {
                    condition: expr(condition),
                    body: Box::new(convert(circuit, body)?),
                };
                converted.push_instruction(instruction).map_err(sim_error)?;
            }
            Instruction::Assign { clbit, value } => {
                let instruction = ketrs::Instruction::Assign {
                    clbit: *clbit,
                    value: expr(value),
                };
                converted.push_instruction(instruction).map_err(sim_error)?;
            }
        }
    }
    Ok(converted)
}

/// A condition as a ketrs classical expression, which is 0 or 1.
fn expr(condition: &Condition) -> ketrs::Expr {
    let binary = |op, a: &Condition, b: &Condition| ketrs::Expr::Binary {
        op,
        left: Box::new(expr(a)),
        right: Box::new(expr(b)),
    };
    match condition {
        Condition::Const(b) => ketrs::Expr::Value(u64::from(*b)),
        Condition::Clbit(k) => ketrs::Expr::Bit(*k),
        Condition::Not(c) => ketrs::Expr::Not(Box::new(expr(c))),
        Condition::And(a, b) => binary(ketrs::BinaryOp::And, a, b),
        Condition::Or(a, b) => binary(ketrs::BinaryOp::Or, a, b),
        Condition::Xor(a, b) => binary(ketrs::BinaryOp::NotEqual, a, b),
    }
}

/// One run on a statevector.
struct KetrsSession {
    state: Statevector,
    rng: SmallRng,
}

impl Session for KetrsSession {
    fn add_qubit(&mut self) -> Result<usize, BackendError> {
        let qubit = self.state.n_qubits();
        if qubit >= MAX_QUBITS {
            return Err(BackendError(format!(
                "more than {MAX_QUBITS} qubits are alive at once, which is beyond the statevector simulator"
            )));
        }
        // The new qubit is the most significant, so existing qubits keep their indices.
        self.state.add_qubit();
        Ok(qubit)
    }

    fn apply(&mut self, op: &Op) -> Result<(), BackendError> {
        self.state.apply(&operation(op)?).map_err(sim_error)
    }

    fn measure(&mut self, qubit: usize) -> Result<bool, BackendError> {
        self.state.measure(qubit, &mut self.rng).map_err(sim_error)
    }

    fn reset(&mut self, qubit: usize) -> Result<(), BackendError> {
        self.state.reset(qubit, &mut self.rng).map_err(sim_error)
    }

    fn release(&mut self, qubit: usize) -> Result<(), BackendError> {
        let p1 = self.state.probability_one(qubit).map_err(sim_error)?;
        if p1 > 1e-9 {
            return Err(BackendError(format!(
                "a released qubit was not in |0⟩ (probability of 1: {p1:.3})"
            )));
        }
        Ok(())
    }

    fn expectation(
        &self,
        terms: &[(f64, Vec<(usize, Pauli)>)],
    ) -> Option<Result<f64, BackendError>> {
        let mut sum = PauliSum::new();
        for (coeff, product) in terms {
            let ops = product.iter().map(|&(q, p)| {
                let pauli = match p {
                    Pauli::X => ketrs::Pauli::X,
                    Pauli::Y => ketrs::Pauli::Y,
                    Pauli::Z => ketrs::Pauli::Z,
                };
                (q, pauli)
            });
            match PauliString::new(ops) {
                Ok(string) => {
                    sum.add(*coeff, string);
                }
                Err(e) => return Some(Err(sim_error(e))),
            }
        }
        Some(self.state.expectation(&sum).map_err(sim_error))
    }

    fn zero_amplitude(&self) -> Option<Result<(f64, f64), BackendError>> {
        // With no qubits the state is the number 1.
        let zero = self
            .state
            .amplitudes()
            .first()
            .map_or((1.0, 0.0), |a| (a.re, a.im));
        Some(Ok(zero))
    }

    fn sample(&mut self, qubits: &[usize], shots: usize) -> Option<Counts> {
        let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
        for (index, count) in self.state.sample_counts(shots, &mut self.rng) {
            let bits = qubits
                .iter()
                .enumerate()
                .fold(0u64, |acc, (k, &q)| acc | (((index >> q) & 1) as u64) << k);
            *counts.entry(bits).or_default() += count;
        }
        Some(counts.into_iter().collect())
    }
}

/// The ketrs operation for a gate with its controls.
fn operation(op: &Op) -> Result<Operation, BackendError> {
    // A two-qubit matrix: ketrs counts its qubits from the least significant, so the first
    // (the left label, most significant) goes second.
    if op.gate == Gate::Unitary && op.targets.len() == 2 {
        if !op.controls.is_empty() {
            return Err(BackendError(
                "a two-qubit matrix gate under `ctrl` is not supported yet".into(),
            ));
        }
        let m = crate::unitary::rows(&op.params, op.adjoint);
        let matrix = std::array::from_fn(|i| {
            std::array::from_fn(|j| ketrs::Complex64::new(m[i][j].0, m[i][j].1))
        });
        return Ok(Operation::Unitary2 {
            matrix,
            qubits: [op.targets[1], op.targets[0]],
        });
    }
    let gate = ketrs_gate(op.gate, op.adjoint, &op.params);
    Ok(
        match (op.controls.as_slice(), gate, op.targets.as_slice()) {
            ([], None, &[a, b]) => Operation::Swap(a, b),
            ([], Some(gate), &[target]) => Operation::Gate { gate, target },
            (&[control], Some(gate), &[target]) => Operation::Controlled {
                gate,
                control,
                target,
            },
            (controls, Some(gate), &[target]) => Operation::MultiControlled {
                gate,
                controls: controls.to_vec(),
                target,
            },
            (controls, None, &[a, b]) => Operation::ControlledSwap {
                controls: controls.to_vec(),
                a,
                b,
            },
            _ => {
                return Err(BackendError(
                    "a gate with the wrong number of qubits".into(),
                ));
            }
        },
    )
}

/// The ketrs gate for a Noev gate, inverted for `adj`; `None` for `swap`.
fn ketrs_gate(gate: Gate, adjoint: bool, params: &[f64]) -> Option<ketrs::Gate> {
    use ketrs::Gate as K;
    let sign = if adjoint { -1.0 } else { 1.0 };
    let param = |i: usize| params.get(i).copied().unwrap_or(0.0);
    Some(match gate {
        Gate::X => K::X,
        Gate::Y => K::Y,
        Gate::Z => K::Z,
        Gate::H => K::H,
        Gate::S if adjoint => K::Sdg,
        Gate::S => K::S,
        Gate::T if adjoint => K::Tdg,
        Gate::T => K::T,
        Gate::Rx => K::Rx(sign * param(0)),
        Gate::Ry => K::Ry(sign * param(0)),
        Gate::Rz => K::Rz(sign * param(0)),
        Gate::P => K::Phase(sign * param(0)),
        // U(θ, φ, λ)† = U(−θ, −λ, −φ).
        Gate::U if adjoint => K::U {
            theta: -param(0),
            phi: -param(2),
            lambda: -param(1),
        },
        Gate::U => K::U {
            theta: param(0),
            phi: param(1),
            lambda: param(2),
        },
        Gate::Swap => return None,
        Gate::Unitary => {
            let m = crate::unitary::rows(params, adjoint);
            let c = |(re, im): (f64, f64)| ketrs::Complex64::new(re, im);
            K::Unitary([[c(m[0][0]), c(m[0][1])], [c(m[1][0]), c(m[1][1])]])
        }
    })
}

fn sim_error(error: ketrs::Error) -> BackendError {
    BackendError(format!("simulator error: {error}"))
}
