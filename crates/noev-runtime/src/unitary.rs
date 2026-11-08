//! Gates given as matrices (`spec/core.md` §1). A matrix travels as its gate's parameters,
//! the real and imaginary part of each entry, rows first: 8 numbers for one qubit, 32 for two.

use noev_sema::prelude::Gate;

use crate::backend::Op;
use crate::value::Value;

/// A complex number as (real, imaginary) parts.
type C = (f64, f64);

fn mul((a, b): C, (c, d): C) -> C {
    (a * c - b * d, a * d + b * c)
}

fn add((a, b): C, (c, d): C) -> C {
    (a + c, b + d)
}

fn conj((a, b): C) -> C {
    (a, -b)
}

fn abs((a, b): C) -> f64 {
    a.hypot(b)
}

fn arg((a, b): C) -> f64 {
    b.atan2(a)
}

/// How close `m·m†` must be to the identity, entry by entry.
const TOLERANCE: f64 = 1e-9;

/// The side of a matrix with these parameters: 2 or 4.
pub fn size(params: &[f64]) -> usize {
    if params.len() == 32 { 4 } else { 2 }
}

/// Entry (`row`, `col`) of an `n`×`n` matrix as (real, imaginary) parts.
fn entry(params: &[f64], n: usize, row: usize, col: usize) -> (f64, f64) {
    let k = 2 * (row * n + col);
    (params[k], params[k + 1])
}

/// The parameters for a matrix value, a `[[Complex; n]; n]`, checked to be unitary.
pub fn params(value: &Value) -> Result<Vec<f64>, String> {
    let Value::Aggregate(rows) = value else {
        return Err(format!("expected a matrix, found {value:?}"));
    };
    let mut params = Vec::with_capacity(2 * rows.len() * rows.len());
    for row in rows {
        let Value::Aggregate(entries) = row else {
            return Err(format!("expected a matrix row, found {row:?}"));
        };
        for entry in entries {
            let Value::Complex(re, im) = entry else {
                return Err(format!("expected a Complex, found {entry:?}"));
            };
            params.extend([*re, *im]);
        }
    }
    let n = rows.len();
    for i in 0..n {
        for j in 0..n {
            // (m·m†)[i][j] = Σ_k m[i][k] · conj(m[j][k]).
            let (mut re, mut im) = (0.0, 0.0);
            for k in 0..n {
                let (a, b) = entry(&params, n, i, k);
                let (c, d) = entry(&params, n, j, k);
                re += a * c + b * d;
                im += b * c - a * d;
            }
            let want = if i == j { 1.0 } else { 0.0 };
            if (re - want).abs() > TOLERANCE || im.abs() > TOLERANCE {
                return Err("this matrix is not unitary".into());
            }
        }
    }
    Ok(params)
}

/// The rows of the matrix, inverted (its conjugate transpose) for `adj`.
pub fn rows(params: &[f64], adjoint: bool) -> Vec<Vec<(f64, f64)>> {
    let n = size(params);
    (0..n)
        .map(|i| {
            (0..n)
                .map(|j| {
                    if adjoint {
                        let (re, im) = entry(params, n, j, i);
                        (re, -im)
                    } else {
                        entry(params, n, i, j)
                    }
                })
                .collect()
        })
        .collect()
}

/// A one-qubit matrix as `e^{iγ} U(θ, φ, λ)`, in OpenQASM's convention:
/// `U = [[cos(θ/2), −e^{iλ} sin(θ/2)], [e^{iφ} sin(θ/2), e^{i(φ+λ)} cos(θ/2)]]`.
/// Returns (θ, φ, λ, γ).
pub fn euler(params: &[f64], adjoint: bool) -> (f64, f64, f64, f64) {
    let m = rows(params, adjoint);
    let abs = |(re, im): (f64, f64)| re.hypot(im);
    let arg = |(re, im): (f64, f64)| im.atan2(re);
    let neg = |(re, im): (f64, f64)| (-re, -im);
    let theta = 2.0 * abs(m[1][0]).atan2(abs(m[0][0]));
    // With cos(θ/2) = 0 the phase γ + λ is all that matters, so take λ = 0.
    let (gamma, lambda) = if abs(m[0][0]) > 1e-12 {
        let gamma = arg(m[0][0]);
        let lambda = if abs(m[0][1]) > 1e-12 {
            arg(neg(m[0][1])) - gamma
        } else {
            arg(m[1][1]) - gamma
        };
        (gamma, lambda)
    } else {
        (arg(neg(m[0][1])), 0.0)
    };
    let phi = if abs(m[1][0]) > 1e-12 {
        arg(m[1][0]) - gamma
    } else {
        arg(m[1][1]) - gamma - lambda
    };
    (theta, phi, lambda, gamma)
}

/// The basis states of two qubits in an order where neighbours differ in one qubit:
/// \|00⟩, \|01⟩, \|11⟩, \|10⟩, as indices with the first qubit most significant.
const CHAIN: [usize; 4] = [0, 1, 3, 2];

/// A one-qubit gate on `target`, controlled by `controls`.
fn op(gate: Gate, params: Vec<f64>, controls: Vec<usize>, target: usize) -> Op {
    Op {
        gate,
        adjoint: false,
        params,
        controls,
        targets: vec![target],
    }
}

/// A two-qubit matrix on the qubits `first` (the left label) and `second`, as gates every
/// backend and OpenQASM have: one-qubit matrices controlled by the other qubit, `x`, and
/// phase gates. In the basis order [`CHAIN`], rotations of neighbouring rows (Givens) reduce
/// the matrix to a diagonal; each rotation acts on two basis states that differ in one qubit,
/// so it is a one-qubit gate controlled by the other, and the diagonal is phase gates. The
/// overall phase is kept, as `p·x·p·x`, since an outer `ctrl` makes it a relative one.
pub fn decompose(params: &[f64], adjoint: bool, first: usize, second: usize) -> Vec<Op> {
    let m = rows(params, adjoint);
    let mut v = [[(0.0, 0.0); 4]; 4];
    for i in 0..4 {
        for j in 0..4 {
            v[i][j] = m[CHAIN[i]][CHAIN[j]];
        }
    }
    // G_k … G_1 · v = d, so v = G_1† … G_k† · d: d runs first, then G_k†, …, G_1†.
    let mut rotations: Vec<(usize, [[C; 2]; 2])> = Vec::new();
    for col in 0..3 {
        for row in (col + 1..4).rev() {
            let (x, y) = (v[row - 1][col], v[row][col]);
            if abs(y) < 1e-15 {
                continue;
            }
            let r = abs(x).hypot(abs(y));
            let scale = |(a, b): C| (a / r, b / r);
            let g = [
                [scale(conj(x)), scale(conj(y))],
                [scale((-y.0, -y.1)), scale(x)],
            ];
            let (upper, lower) = v.split_at_mut(row);
            for (p, q) in upper[row - 1].iter_mut().zip(lower[0].iter_mut()) {
                (*p, *q) = (
                    add(mul(g[0][0], *p), mul(g[0][1], *q)),
                    add(mul(g[1][0], *p), mul(g[1][1], *q)),
                );
            }
            rotations.push((row - 1, g));
        }
    }
    let mut ops = Vec::new();
    // The diagonal: an overall phase, a phase on each qubit's 1, and a controlled phase.
    let mut d = [(0.0, 0.0); 4];
    for i in 0..4 {
        d[CHAIN[i]] = v[i][i];
    }
    let phase = |c: C| arg(mul(c, conj(d[0])));
    let (whole, on_second, on_first, both) = (arg(d[0]), phase(d[1]), phase(d[2]), phase(d[3]));
    let p = |angle: f64, controls: Vec<usize>, target| op(Gate::P, vec![angle], controls, target);
    if whole.abs() > 1e-15 {
        // e^{iφ}·I = X · P(φ) · X · P(φ).
        for _ in 0..2 {
            ops.push(p(whole, vec![], first));
            ops.push(op(Gate::X, vec![], vec![], first));
        }
    }
    for (angle, controls, target) in [
        (on_second, vec![], second),
        (on_first, vec![], first),
        (both - on_first - on_second, vec![first], second),
    ] {
        if angle.abs() > 1e-15 {
            ops.push(p(angle, controls, target));
        }
    }
    for (row, g) in rotations.into_iter().rev() {
        let (s, t) = (CHAIN[row], CHAIN[row + 1]);
        // The qubit that differs, the other, and the value the other has in both states.
        let (target, other, value) = if (s ^ t) == 1 {
            (second, first, s >> 1)
        } else {
            (first, second, s & 1)
        };
        // G† in the basis (s, t), swapped if s has the target qubit at 1.
        let dagger = [
            [conj(g[0][0]), conj(g[1][0])],
            [conj(g[0][1]), conj(g[1][1])],
        ];
        let swapped = (if target == second { s & 1 } else { s >> 1 }) == 1;
        let u = |i: usize, j: usize| {
            if swapped {
                dagger[1 - i][1 - j]
            } else {
                dagger[i][j]
            }
        };
        let params = vec![
            u(0, 0).0,
            u(0, 0).1,
            u(0, 1).0,
            u(0, 1).1,
            u(1, 0).0,
            u(1, 0).1,
            u(1, 1).0,
            u(1, 1).1,
        ];
        let flip = value == 0;
        if flip {
            ops.push(op(Gate::X, vec![], vec![], other));
        }
        ops.push(op(Gate::Unitary, params, vec![other], target));
        if flip {
            ops.push(op(Gate::X, vec![], vec![], other));
        }
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matrix(entries: &[(f64, f64)]) -> Value {
        let n = (entries.len() as f64).sqrt() as usize;
        Value::Aggregate(
            entries
                .chunks(n)
                .map(|row| {
                    Value::Aggregate(row.iter().map(|&(re, im)| Value::Complex(re, im)).collect())
                })
                .collect(),
        )
    }

    /// The matrix `ops` apply to two qubits, 0 (the first, most significant) and 1.
    fn apply(ops: &[Op]) -> [[C; 4]; 4] {
        let bit = |index: usize, q: usize| if q == 0 { index >> 1 & 1 } else { index & 1 };
        let mut columns = [[(0.0, 0.0); 4]; 4];
        for (j, column) in columns.iter_mut().enumerate() {
            let mut state = [(0.0, 0.0); 4];
            state[j] = (1.0, 0.0);
            for op in ops {
                let target = op.targets[0];
                let m = match op.gate {
                    Gate::X => [[(0.0, 0.0), (1.0, 0.0)], [(1.0, 0.0), (0.0, 0.0)]],
                    Gate::P => [
                        [(1.0, 0.0), (0.0, 0.0)],
                        [(0.0, 0.0), (op.params[0].cos(), op.params[0].sin())],
                    ],
                    Gate::Unitary => {
                        let r = rows(&op.params, false);
                        [[r[0][0], r[0][1]], [r[1][0], r[1][1]]]
                    }
                    other => panic!("unexpected {other:?}"),
                };
                let mut next = state;
                for (i, slot) in next.iter_mut().enumerate() {
                    if op.controls.iter().any(|&c| bit(i, c) == 0) {
                        continue;
                    }
                    let partner = i ^ if target == 0 { 2 } else { 1 };
                    let (lo, hi) = if bit(i, target) == 0 {
                        (i, partner)
                    } else {
                        (partner, i)
                    };
                    let row = bit(i, target);
                    *slot = add(mul(m[row][0], state[lo]), mul(m[row][1], state[hi]));
                }
                state = next;
            }
            *column = state;
        }
        // columns[j][i] is entry (i, j).
        let mut out = [[(0.0, 0.0); 4]; 4];
        for i in 0..4 {
            for j in 0..4 {
                out[i][j] = columns[j][i];
            }
        }
        out
    }

    fn flat(m: &[[C; 4]; 4]) -> Vec<f64> {
        m.iter().flatten().flat_map(|&(re, im)| [re, im]).collect()
    }

    fn matmul(a: &[[C; 4]; 4], b: &[[C; 4]; 4]) -> [[C; 4]; 4] {
        let mut out = [[(0.0, 0.0); 4]; 4];
        for i in 0..4 {
            for j in 0..4 {
                for k in 0..4 {
                    out[i][j] = add(out[i][j], mul(a[i][k], b[k][j]));
                }
            }
        }
        out
    }

    #[test]
    fn two_qubit_matrices_decompose_exactly() {
        let (o, z, i) = ((1.0, 0.0), (0.0, 0.0), (0.0, 1.0));
        let cnot = [[o, z, z, z], [z, o, z, z], [z, z, z, o], [z, z, o, z]];
        let swap = [[o, z, z, z], [z, z, o, z], [z, o, z, z], [z, z, z, o]];
        let iswap = [[o, z, z, z], [z, z, i, z], [z, i, z, z], [z, z, z, o]];
        let phases = [
            [i, z, z, z],
            [z, (-1.0, 0.0), z, z],
            [z, z, (0.0, -1.0), z],
            [z, z, z, o],
        ];
        // H ⊗ H, iSWAP, CNOT and phases: a dense matrix with every entry set.
        let hh = [
            [(0.5, 0.0), (0.5, 0.0), (0.5, 0.0), (0.5, 0.0)],
            [(0.5, 0.0), (-0.5, 0.0), (0.5, 0.0), (-0.5, 0.0)],
            [(0.5, 0.0), (0.5, 0.0), (-0.5, 0.0), (-0.5, 0.0)],
            [(0.5, 0.0), (-0.5, 0.0), (-0.5, 0.0), (0.5, 0.0)],
        ];
        let dense = matmul(&phases, &matmul(&cnot, &matmul(&iswap, &hh)));
        for (name, matrix) in [
            ("cnot", cnot),
            ("swap", swap),
            ("iswap", iswap),
            ("phases", phases),
            ("dense", dense),
        ] {
            let params = params(&Value::Aggregate(
                matrix
                    .iter()
                    .map(|row| {
                        Value::Aggregate(
                            row.iter().map(|&(re, im)| Value::Complex(re, im)).collect(),
                        )
                    })
                    .collect(),
            ))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(params, flat(&matrix));
            for adjoint in [false, true] {
                let got = apply(&decompose(&params, adjoint, 0, 1));
                let want = rows(&params, adjoint);
                for r in 0..4 {
                    for c in 0..4 {
                        let (g, w) = (got[r][c], want[r][c]);
                        assert!(
                            (g.0 - w.0).abs() < 1e-12 && (g.1 - w.1).abs() < 1e-12,
                            "{name} (adjoint {adjoint}) at ({r}, {c}): {g:?} not {w:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn unitarity_and_euler_angles() {
        let r = std::f64::consts::FRAC_1_SQRT_2;
        let h = params(&matrix(&[(r, 0.0), (r, 0.0), (r, 0.0), (-r, 0.0)])).unwrap();
        // H = U(π/2, 0, π).
        let (theta, phi, lambda, gamma) = euler(&h, false);
        let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
        assert!(
            close(theta, std::f64::consts::FRAC_PI_2) && close(phi, 0.0),
            "{theta} {phi}"
        );
        assert!(close(
            lambda.rem_euclid(std::f64::consts::TAU),
            std::f64::consts::PI
        ));
        assert!(close(gamma, 0.0));
        // S† = diag(1, −i): its adjoint is S.
        let s = params(&matrix(&[(1.0, 0.0), (0.0, 0.0), (0.0, 0.0), (0.0, 1.0)])).unwrap();
        assert_eq!(rows(&s, true)[1][1], (0.0, -1.0));
        assert!(params(&matrix(&[(1.0, 0.0), (1.0, 0.0), (0.0, 0.0), (1.0, 0.0)])).is_err());
    }
}
