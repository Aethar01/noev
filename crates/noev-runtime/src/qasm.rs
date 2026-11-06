//! Writing a [`Circuit`] as OpenQASM 3.
//!
//! Gates use the names in OpenQASM's `stdgates.inc`. `adj` is written out (`sdg`, a negated
//! angle) rather than with the `inv @` modifier, and `ctrl` becomes the `ctrl(n) @` modifier.

use std::fmt::Write;

use noev_sema::prelude::Gate;

use crate::backend::{Circuit, Condition, Instruction, Op};

/// The OpenQASM 3 program for `circuit`. `header` lines go first, as comments.
pub fn to_qasm3(circuit: &Circuit, header: &[String]) -> String {
    let mut out = String::new();
    for line in header {
        let _ = writeln!(out, "// {line}");
    }
    out.push_str("OPENQASM 3.0;\ninclude \"stdgates.inc\";\n\n");
    let _ = writeln!(out, "qubit[{}] q;", circuit.qubits);
    if circuit.clbits > 0 {
        let _ = writeln!(out, "bit[{}] c;", circuit.clbits);
    }
    out.push('\n');
    instructions(&mut out, &circuit.instructions, 0);
    out
}

fn instructions(out: &mut String, list: &[Instruction], depth: usize) {
    let indent = "    ".repeat(depth);
    for instruction in list {
        out.push_str(&indent);
        match instruction {
            Instruction::Gate(op) => gate(out, op),
            Instruction::Measure { qubit, clbit } => {
                let _ = writeln!(out, "c[{clbit}] = measure q[{qubit}];");
            }
            Instruction::Reset(qubit) => {
                let _ = writeln!(out, "reset q[{qubit}];");
            }
            Instruction::If {
                condition,
                then,
                otherwise,
            } => {
                let _ = writeln!(out, "if ({}) {{", condition_text(condition));
                instructions(out, then, depth + 1);
                if otherwise.is_empty() {
                    let _ = writeln!(out, "{indent}}}");
                } else {
                    let _ = writeln!(out, "{indent}}} else {{");
                    instructions(out, otherwise, depth + 1);
                    let _ = writeln!(out, "{indent}}}");
                }
            }
            Instruction::While { condition, body } => {
                let _ = writeln!(out, "while ({}) {{", condition_text(condition));
                instructions(out, body, depth + 1);
                let _ = writeln!(out, "{indent}}}");
            }
            Instruction::Assign { clbit, value } => {
                let _ = writeln!(out, "c[{clbit}] = {};", condition_text(value));
            }
        }
    }
}

fn condition_text(condition: &Condition) -> String {
    // Operands that are themselves operations go in brackets.
    let operand = |c: &Condition| match c {
        Condition::Const(_) | Condition::Clbit(_) => condition_text(c),
        Condition::Not(inner) if matches!(**inner, Condition::Clbit(_)) => condition_text(c),
        _ => format!("({})", condition_text(c)),
    };
    match condition {
        Condition::Const(b) => b.to_string(),
        Condition::Clbit(k) => format!("c[{k}] == 1"),
        Condition::Not(c) => match **c {
            Condition::Clbit(k) => format!("c[{k}] == 0"),
            _ => format!("!{}", operand(c)),
        },
        Condition::And(a, b) => format!("{} && {}", operand(a), operand(b)),
        Condition::Or(a, b) => format!("{} || {}", operand(a), operand(b)),
        Condition::Xor(a, b) => format!("{} != {}", operand(a), operand(b)),
    }
}

fn gate(out: &mut String, op: &Op) {
    let angle = |i: usize| {
        let x = op.params.get(i).copied().unwrap_or(0.0);
        if op.adjoint { -x } else { x }
    };
    let call = match op.gate {
        Gate::X => "x".to_string(),
        Gate::Y => "y".to_string(),
        Gate::Z => "z".to_string(),
        Gate::H => "h".to_string(),
        Gate::Swap => "swap".to_string(),
        Gate::S if op.adjoint => "sdg".to_string(),
        Gate::S => "s".to_string(),
        Gate::T if op.adjoint => "tdg".to_string(),
        Gate::T => "t".to_string(),
        Gate::Rx => format!("rx({})", number(angle(0))),
        Gate::Ry => format!("ry({})", number(angle(0))),
        Gate::Rz => format!("rz({})", number(angle(0))),
        Gate::P => format!("p({})", number(angle(0))),
        // U(θ, φ, λ)† = U(−θ, −λ, −φ).
        Gate::U => {
            let p = |i: usize| op.params.get(i).copied().unwrap_or(0.0);
            let (theta, phi, lambda) = if op.adjoint {
                (-p(0), -p(2), -p(1))
            } else {
                (p(0), p(1), p(2))
            };
            format!("U({}, {}, {})", number(theta), number(phi), number(lambda))
        }
    };
    match op.controls.len() {
        0 => {}
        1 => out.push_str("ctrl @ "),
        n => {
            let _ = write!(out, "ctrl({n}) @ ");
        }
    }
    out.push_str(&call);
    for (i, q) in op.controls.iter().chain(&op.targets).enumerate() {
        out.push_str(if i == 0 { " " } else { ", " });
        let _ = write!(out, "q[{q}]");
    }
    out.push_str(";\n");
}

/// A float that reads back exactly, with a decimal point.
fn number(x: f64) -> String {
    format!("{x:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(gate: Gate, params: &[f64], controls: &[usize], targets: &[usize]) -> Instruction {
        Instruction::Gate(Op {
            gate,
            adjoint: false,
            params: params.to_vec(),
            controls: controls.to_vec(),
            targets: targets.to_vec(),
        })
    }

    #[test]
    fn bell_pair() {
        let circuit = Circuit {
            qubits: 2,
            clbits: 2,
            instructions: vec![
                op(Gate::H, &[], &[], &[0]),
                op(Gate::X, &[], &[0], &[1]),
                Instruction::Measure { qubit: 0, clbit: 0 },
                Instruction::Measure { qubit: 1, clbit: 1 },
            ],
        };
        assert_eq!(
            to_qasm3(&circuit, &["shots: 1000".into()]),
            "// shots: 1000\nOPENQASM 3.0;\ninclude \"stdgates.inc\";\n\nqubit[2] q;\nbit[2] c;\n\n\
             h q[0];\nctrl @ x q[0], q[1];\nc[0] = measure q[0];\nc[1] = measure q[1];\n"
        );
    }

    #[test]
    fn adjoints_controls_and_angles() {
        let mut sdg = op(Gate::S, &[], &[], &[0]);
        let mut ry = op(Gate::Ry, &[0.5], &[], &[1]);
        for i in [&mut sdg, &mut ry] {
            if let Instruction::Gate(op) = i {
                op.adjoint = true;
            }
        }
        let circuit = Circuit {
            qubits: 3,
            clbits: 0,
            instructions: vec![
                sdg,
                ry,
                op(Gate::X, &[], &[0, 1], &[2]),
                op(Gate::Swap, &[], &[0], &[1, 2]),
                op(Gate::P, &[1.0], &[], &[0]),
                Instruction::Reset(2),
            ],
        };
        let text = to_qasm3(&circuit, &[]);
        let body: Vec<&str> = text.lines().skip(5).collect();
        assert_eq!(
            body,
            [
                "sdg q[0];",
                "ry(-0.5) q[1];",
                "ctrl(2) @ x q[0], q[1], q[2];",
                "ctrl @ swap q[0], q[1], q[2];",
                "p(1.0) q[0];",
                "reset q[2];",
            ]
        );
    }
}
