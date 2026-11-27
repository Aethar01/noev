//! QIR 1.0 emission throughput benchmark (Base and Adaptive profiles).
#![allow(missing_docs)]

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use noev_runtime::backend::{Circuit, Condition, Instruction, Op};
use noev_runtime::qir;
use noev_sema::prelude::Gate;

fn op(gate: Gate, params: &[f64], controls: &[usize], targets: &[usize]) -> Instruction {
    Instruction::Gate(Op {
        gate,
        adjoint: false,
        params: params.to_vec(),
        controls: controls.to_vec(),
        targets: targets.to_vec(),
    })
}

fn base_circuit(layers: usize, qubits: usize) -> Circuit {
    let mut instructions = Vec::with_capacity(layers * qubits * 2 + qubits);
    for l in 0..layers {
        for q in 0..qubits {
            instructions.push(op(Gate::Ry, &[0.1 * (l + q + 1) as f64], &[], &[q]));
        }
        for q in 0..qubits - 1 {
            instructions.push(op(Gate::X, &[], &[q], &[q + 1]));
        }
    }
    for q in 0..qubits {
        instructions.push(Instruction::Measure { qubit: q, clbit: q });
    }
    Circuit {
        qubits,
        clbits: qubits,
        instructions,
    }
}

fn adaptive_circuit(rounds: usize) -> Circuit {
    let mut instructions = Vec::new();
    for r in 0..rounds {
        let q = r % 4;
        instructions.push(op(Gate::H, &[], &[], &[q]));
        instructions.push(op(Gate::X, &[], &[q], &[(q + 1) % 4]));
        instructions.push(Instruction::Measure { qubit: q, clbit: 0 });
        instructions.push(Instruction::If {
            condition: Condition::Clbit(0),
            then: vec![op(Gate::Z, &[], &[], &[(q + 1) % 4]), Instruction::Reset(q)],
            otherwise: vec![],
        });
    }
    Circuit {
        qubits: 4,
        clbits: 1,
        instructions,
    }
}

fn bench_qir(c: &mut Criterion) {
    let base = base_circuit(50, 16);
    let base_ops = base.instructions.len() as u64;
    let adaptive = adaptive_circuit(250);
    let adaptive_ops = adaptive.instructions.len() as u64;

    let mut group = c.benchmark_group("qir");
    group.throughput(Throughput::Elements(base_ops));
    group.bench_function("base_profile", |b| {
        b.iter(|| black_box(qir::to_qir(black_box(&base), "main", &[])))
    });
    group.throughput(Throughput::Elements(adaptive_ops));
    group.bench_function("adaptive_profile", |b| {
        b.iter(|| black_box(qir::to_qir(black_box(&adaptive), "main", &[])))
    });
    group.finish();
}

criterion_group!(benches, bench_qir);
criterion_main!(benches);
