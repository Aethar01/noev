//! Throughput benchmarks for the Runtime C ABI (`spec/runtime.md` §4).
//! Run with `cargo bench -p noev-runtime`.

use std::hint::black_box;
use std::time::{Duration, Instant};

use noev_runtime::cabi::*;

fn fastest(runs: usize, mut f: impl FnMut()) -> Duration {
    (0..runs)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed()
        })
        .min()
        .unwrap()
}

fn main() {
    // 1. Gate-level C ABI throughput: Bell-pair prep + uncompute + release on ketrs.
    let rounds = 20_000usize;
    let gates_per_round = 4usize; // H, CNOT, CNOT, H
    let time = fastest(5, || unsafe {
        let rt = noev_rt_new_ketrs(1);
        let mut q0 = NoevQubit(0);
        let mut q1 = NoevQubit(0);
        assert!(noev_rt_qubit_allocate(rt, &mut q0));
        assert!(noev_rt_qubit_allocate(rt, &mut q1));
        for _ in 0..rounds {
            black_box(noev_qis_h(rt, q0));
            noev_rt_ctrl_push(rt, &q0, 1);
            black_box(noev_qis_x(rt, q1));
            black_box(noev_qis_x(rt, q1));
            noev_rt_ctrl_pop(rt);
            black_box(noev_qis_h(rt, q0));
        }
        assert!(noev_rt_qubit_release(rt, q1));
        assert!(noev_rt_qubit_release(rt, q0));
        noev_rt_free(rt);
    });
    println!(
        "{:<14} {:>10.0} gates/s  ({:.2} ms for {} gates)",
        "cabi_sim",
        (rounds * gates_per_round) as f64 / time.as_secs_f64(),
        time.as_secs_f64() * 1e3,
        rounds * gates_per_round,
    );

    // 2. Circuit-recording C ABI throughput: recording gates in Build mode.
    let rec_gates = 100_000usize;
    let time = fastest(10, || unsafe {
        let rt = noev_rt_new_export(1);
        let mut q0 = NoevQubit(0);
        let mut q1 = NoevQubit(0);
        assert!(noev_rt_qubit_allocate(rt, &mut q0));
        assert!(noev_rt_qubit_allocate(rt, &mut q1));
        for _ in 0..(rec_gates / 2) {
            black_box(noev_qis_h(rt, q0));
            noev_rt_ctrl_push(rt, &q0, 1);
            black_box(noev_qis_x(rt, q1));
            noev_rt_ctrl_pop(rt);
        }
        noev_rt_free(rt);
    });
    println!(
        "{:<14} {:>10.0} gates/s  ({:.2} ms for {} recorded gates)",
        "cabi_record",
        rec_gates as f64 / time.as_secs_f64(),
        time.as_secs_f64() * 1e3,
        rec_gates,
    );

    // 3. Inline OpenQASM C ABI throughput: simulation (ketrs) and circuit recording.
    let qasm_snippet = b"sx \x010\x01; rz(0.5) \x010\x01; cx \x010\x01, \x011\x01;";
    let qasm_sim_calls = 5_000usize;
    let time = fastest(5, || unsafe {
        let rt = noev_rt_new_ketrs(1);
        let mut q0 = NoevQubit(0);
        let mut q1 = NoevQubit(0);
        assert!(noev_rt_qubit_allocate(rt, &mut q0));
        assert!(noev_rt_qubit_allocate(rt, &mut q1));
        let qs = [q0, q1];
        for _ in 0..qasm_sim_calls {
            black_box(noev_qis_qasm(
                rt,
                qasm_snippet.as_ptr(),
                qasm_snippet.len(),
                qs.as_ptr(),
                qs.len(),
            ));
        }
        noev_rt_free(rt);
    });
    println!(
        "{:<14} {:>10.0} calls/s  ({:.2} ms for {} inline qasm sim calls)",
        "cabi_qasm_sim",
        qasm_sim_calls as f64 / time.as_secs_f64(),
        time.as_secs_f64() * 1e3,
        qasm_sim_calls,
    );

    let qasm_rec_calls = 50_000usize;
    let time = fastest(10, || unsafe {
        let rt = noev_rt_new_export(1);
        let mut q0 = NoevQubit(0);
        let mut q1 = NoevQubit(0);
        assert!(noev_rt_qubit_allocate(rt, &mut q0));
        assert!(noev_rt_qubit_allocate(rt, &mut q1));
        let qs = [q0, q1];
        for _ in 0..qasm_rec_calls {
            black_box(noev_qis_qasm(
                rt,
                qasm_snippet.as_ptr(),
                qasm_snippet.len(),
                qs.as_ptr(),
                qs.len(),
            ));
        }
        noev_rt_free(rt);
    });
    println!(
        "{:<14} {:>10.0} calls/s  ({:.2} ms for {} inline qasm recorded calls)",
        "cabi_qasm_rec",
        qasm_rec_calls as f64 / time.as_secs_f64(),
        time.as_secs_f64() * 1e3,
        qasm_rec_calls,
    );

    // 4. Circuit-level ketrs backend: 16-qubit real circuit with redundant gates (tests
    //    Circuit::optimize + RealStatevector dispatch).
    use noev_runtime::backend::{Backend, Circuit, Instruction, Op};
    use noev_sema::prelude::Gate;
    let n = 16usize;
    let mut instructions = Vec::new();
    for q in 0..n {
        instructions.push(Instruction::Gate(Op {
            gate: Gate::Ry,
            adjoint: false,
            params: vec![0.3],
            controls: vec![],
            targets: vec![q],
        }));
        instructions.push(Instruction::Gate(Op {
            gate: Gate::Ry,
            adjoint: false,
            params: vec![0.4],
            controls: vec![],
            targets: vec![q],
        }));
    }
    for q in 0..(n - 1) {
        instructions.push(Instruction::Gate(Op {
            gate: Gate::X,
            adjoint: false,
            params: vec![],
            controls: vec![q],
            targets: vec![q + 1],
        }));
    }
    for q in 0..n {
        instructions.push(Instruction::Measure { qubit: q, clbit: q });
    }
    let circuit = Circuit {
        qubits: n,
        clbits: n,
        instructions,
    };
    let time = fastest(5, || {
        black_box(
            noev_runtime::Ketrs
                .run_circuit(&circuit, 10_000, 7)
                .unwrap()
                .unwrap(),
        );
    });
    println!(
        "{:<14} {:>10.2} ms  (16q real circuit, 10000 shots)",
        "ketrs_16q_real",
        time.as_secs_f64() * 1e3,
    );
}
