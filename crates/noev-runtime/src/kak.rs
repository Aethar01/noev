//! The fewest CNOTs for a two-qubit gate (`spec/core.md` §1): the KAK decomposition.
//!
//! Every two-qubit unitary is `(A₁⊗B₁) · N(a, b, c) · (A₂⊗B₂)` up to a phase, where
//! `N(a, b, c) = exp(i(a·XX + b·YY + c·ZZ))`. In the magic basis local gates are real
//! orthogonal matrices and `N` is diagonal, so diagonalising `Uᵀ·U` there separates them.
//! Each coefficient is then brought into (−π/4, π/4] by moving whole multiples of π/2 into
//! the local gates, and `N` takes 0 CNOTs if all three are 0, 1 if it is a CNOT's (one is
//! ±π/4, the others 0), 2 if one is 0, and 3 otherwise: the fewest possible.
//!
//! Qubit 0 is the first (the most significant bit of a basis state's index), qubit 1 the
//! second.

// Dense 4×4 matrix arithmetic reads most clearly with indices.
#![allow(clippy::needless_range_loop)]

use std::f64::consts::{FRAC_PI_2, FRAC_PI_4};

/// A complex number as (real, imaginary) parts.
pub type C = (f64, f64);
type M2 = [[C; 2]; 2];
type M4 = [[C; 4]; 4];

/// A gate of a decomposition.
#[derive(Clone, Debug, PartialEq)]
pub enum Piece {
    /// A one-qubit matrix on qubit 0 or 1.
    One(usize, M2),
    /// A CNOT: control, target.
    Cx(usize, usize),
}

const ZERO: C = (0.0, 0.0);
const ONE: C = (1.0, 0.0);
const EPS: f64 = 1e-9;

fn add(a: C, b: C) -> C {
    (a.0 + b.0, a.1 + b.1)
}
fn mul(a: C, b: C) -> C {
    (a.0 * b.0 - a.1 * b.1, a.0 * b.1 + a.1 * b.0)
}
fn conj(a: C) -> C {
    (a.0, -a.1)
}
fn scale(a: C, s: f64) -> C {
    (a.0 * s, a.1 * s)
}
fn abs(a: C) -> f64 {
    a.0.hypot(a.1)
}
fn arg(a: C) -> f64 {
    a.1.atan2(a.0)
}
fn cis(t: f64) -> C {
    (t.cos(), t.sin())
}
fn div(a: C, b: C) -> C {
    let d = b.0 * b.0 + b.1 * b.1;
    scale(mul(a, conj(b)), 1.0 / d)
}

fn mul4(a: &M4, b: &M4) -> M4 {
    let mut out = [[ZERO; 4]; 4];
    for i in 0..4 {
        for j in 0..4 {
            for k in 0..4 {
                out[i][j] = add(out[i][j], mul(a[i][k], b[k][j]));
            }
        }
    }
    out
}

fn mul2(a: &M2, b: &M2) -> M2 {
    let mut out = [[ZERO; 2]; 2];
    for i in 0..2 {
        for j in 0..2 {
            out[i][j] = add(mul(a[i][0], b[0][j]), mul(a[i][1], b[1][j]));
        }
    }
    out
}

fn dagger4(a: &M4) -> M4 {
    let mut out = [[ZERO; 4]; 4];
    for i in 0..4 {
        for j in 0..4 {
            out[i][j] = conj(a[j][i]);
        }
    }
    out
}

fn dagger2(a: &M2) -> M2 {
    [
        [conj(a[0][0]), conj(a[1][0])],
        [conj(a[0][1]), conj(a[1][1])],
    ]
}

fn identity4() -> M4 {
    let mut out = [[ZERO; 4]; 4];
    for (i, row) in out.iter_mut().enumerate() {
        row[i] = ONE;
    }
    out
}

/// A ⊗ B, with A on qubit 0.
fn kron(a: &M2, b: &M2) -> M4 {
    let mut out = [[ZERO; 4]; 4];
    for i in 0..4 {
        for j in 0..4 {
            out[i][j] = mul(a[i >> 1][j >> 1], b[i & 1][j & 1]);
        }
    }
    out
}

fn det4(m: &M4) -> C {
    let mut a = *m;
    let mut det = ONE;
    for col in 0..4 {
        let pivot = (col..4)
            .max_by(|&x, &y| abs(a[x][col]).total_cmp(&abs(a[y][col])))
            .expect("rows remain");
        if abs(a[pivot][col]) < 1e-300 {
            return ZERO;
        }
        if pivot != col {
            a.swap(pivot, col);
            det = scale(det, -1.0);
        }
        det = mul(det, a[col][col]);
        for row in col + 1..4 {
            let f = div(a[row][col], a[col][col]);
            for k in col..4 {
                a[row][k] = add(a[row][k], scale(mul(f, a[col][k]), -1.0));
            }
        }
    }
    det
}

/// The magic basis, as columns: local gates are real orthogonal in it.
fn magic() -> M4 {
    let r = std::f64::consts::FRAC_1_SQRT_2;
    let (o, i, z) = ((r, 0.0), (0.0, r), ZERO);
    [
        [o, z, z, i],
        [z, i, o, z],
        [z, i, scale(o, -1.0), z],
        [o, z, z, scale(i, -1.0)],
    ]
}

/// The eigenvectors of a real symmetric matrix, as columns, by cyclic Jacobi rotations.
fn jacobi(mut a: [[f64; 4]; 4]) -> [[f64; 4]; 4] {
    let mut v = [[0.0; 4]; 4];
    for (i, row) in v.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    for _ in 0..100 {
        let off: f64 = (0..4)
            .flat_map(|i| (0..4).map(move |j| (i, j)))
            .filter(|&(i, j)| i != j)
            .map(|(i, j)| a[i][j] * a[i][j])
            .sum();
        if off < 1e-30 {
            break;
        }
        for p in 0..4 {
            for q in p + 1..4 {
                if a[p][q].abs() < 1e-300 {
                    continue;
                }
                let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
                let t = if theta == 0.0 {
                    1.0
                } else {
                    theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt())
                };
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;
                // a = Jᵀ a J and v = v J for the rotation J in the (p, q) plane.
                for k in 0..4 {
                    let (akp, akq) = (a[k][p], a[k][q]);
                    a[k][p] = c * akp - s * akq;
                    a[k][q] = s * akp + c * akq;
                }
                for k in 0..4 {
                    let (apk, aqk) = (a[p][k], a[q][k]);
                    a[p][k] = c * apk - s * aqk;
                    a[q][k] = s * apk + c * aqk;
                }
                for row in &mut v {
                    let (vp, vq) = (row[p], row[q]);
                    row[p] = c * vp - s * vq;
                    row[q] = s * vp + c * vq;
                }
            }
        }
    }
    v
}

/// The factors of `l = a ⊗ b`, from its largest 2×2 block.
fn factor(l: &M4) -> (M2, M2) {
    let block = |i: usize, j: usize| -> M2 {
        [
            [l[2 * i][2 * j], l[2 * i][2 * j + 1]],
            [l[2 * i + 1][2 * j], l[2 * i + 1][2 * j + 1]],
        ]
    };
    let norm = |m: &M2| m.iter().flatten().map(|&x| abs(x) * abs(x)).sum::<f64>();
    let (bi, bj) = [(0, 0), (0, 1), (1, 0), (1, 1)]
        .into_iter()
        .max_by(|&(a, b), &(c, d)| norm(&block(a, b)).total_cmp(&norm(&block(c, d))))
        .expect("four blocks");
    let mut b = block(bi, bj);
    let d = {
        let (p, q) = (mul(b[0][0], b[1][1]), mul(b[0][1], b[1][0]));
        abs(add(p, scale(q, -1.0))).sqrt()
    };
    for x in b.iter_mut().flatten() {
        *x = scale(*x, 1.0 / d);
    }
    let bd = dagger2(&b);
    let mut a = [[ZERO; 2]; 2];
    for (r, row) in a.iter_mut().enumerate() {
        for (s, entry) in row.iter_mut().enumerate() {
            let m = mul2(&block(r, s), &bd);
            *entry = scale(add(m[0][0], m[1][1]), 0.5);
        }
    }
    (a, b)
}

fn rz(t: f64) -> M2 {
    [[cis(-t / 2.0), ZERO], [ZERO, cis(t / 2.0)]]
}
fn ry(t: f64) -> M2 {
    let (c, s) = ((t / 2.0).cos(), (t / 2.0).sin());
    [[(c, 0.0), (-s, 0.0)], [(s, 0.0), (c, 0.0)]]
}
fn rx(t: f64) -> M2 {
    let (c, s) = ((t / 2.0).cos(), (t / 2.0).sin());
    [[(c, 0.0), (0.0, -s)], [(0.0, -s), (c, 0.0)]]
}
fn hadamard() -> M2 {
    let r = std::f64::consts::FRAC_1_SQRT_2;
    [[(r, 0.0), (r, 0.0)], [(r, 0.0), (-r, 0.0)]]
}
fn s_gate() -> M2 {
    [[ONE, ZERO], [ZERO, (0.0, 1.0)]]
}

/// i·XX, i·YY or i·ZZ: what moving π/2 out of a coefficient leaves behind.
fn i_pauli_pair(k: usize) -> M4 {
    let p: M2 = match k {
        0 => [[ZERO, ONE], [ONE, ZERO]],
        1 => [[ZERO, (0.0, -1.0)], [(0.0, 1.0), ZERO]],
        _ => [[ONE, ZERO], [ZERO, (-1.0, 0.0)]],
    };
    let mut pp = kron(&p, &p);
    for x in pp.iter_mut().flatten() {
        *x = mul(*x, (0.0, 1.0));
    }
    pp
}

/// U = L₁ · N(a, b, c) · L₂ up to a phase, with L₁ and L₂ local.
fn split(u: &M4) -> Option<(M4, [f64; 3], M4)> {
    let b = magic();
    let mut up = mul4(&dagger4(&b), &mul4(u, &b));
    let det = det4(&up);
    let root = scale(cis(-arg(det) / 4.0), 1.0 / abs(det).powf(0.25));
    for x in up.iter_mut().flatten() {
        *x = mul(*x, root);
    }
    // M = Upᵀ·Up is symmetric and unitary: its real and imaginary parts commute, so a real
    // orthogonal P diagonalises both; a mixture of them almost surely has distinct
    // eigenvalues.
    let mut m = [[ZERO; 4]; 4];
    for i in 0..4 {
        for j in 0..4 {
            for k in 0..4 {
                m[i][j] = add(m[i][j], mul(up[k][i], up[k][j]));
            }
        }
    }
    let mut found = None;
    for t in [0.37f64, 1.21, 2.3, 0.9, 1.7] {
        let mut mix = [[0.0; 4]; 4];
        for i in 0..4 {
            for j in 0..4 {
                mix[i][j] = t.cos() * m[i][j].0 + t.sin() * m[i][j].1;
            }
        }
        let p = jacobi(mix);
        let mut d = [[ZERO; 4]; 4];
        for i in 0..4 {
            for j in 0..4 {
                for k in 0..4 {
                    for l in 0..4 {
                        d[i][j] = add(d[i][j], scale(m[k][l], p[k][i] * p[l][j]));
                    }
                }
            }
        }
        let off = (0..4)
            .flat_map(|i| (0..4).map(move |j| (i, j)))
            .filter(|&(i, j)| i != j)
            .map(|(i, j)| abs(d[i][j]))
            .fold(0.0, f64::max);
        if off < 1e-10 {
            found = Some((p, d));
            break;
        }
    }
    let (mut p, d) = found?;
    // A determinant of 1, so the local factors are proper rotations.
    let det_p = {
        let pc: M4 = std::array::from_fn(|i| std::array::from_fn(|j| (p[i][j], 0.0)));
        det4(&pc).0
    };
    if det_p < 0.0 {
        for row in &mut p {
            row[0] = -row[0];
        }
    }
    let mut theta: [f64; 4] = std::array::from_fn(|k| arg(d[k][k]) / 2.0);
    let product = theta.iter().fold(ONE, |acc, &t| mul(acc, cis(t)));
    if abs(add(product, (-1.0, 0.0))) > 1e-6 {
        theta[0] += std::f64::consts::PI;
    }
    // K₁ = Up · P · diag(e^{−iθ}), K₂ = Pᵀ; back in the computational basis, L = B·K·B†.
    let pc: M4 = std::array::from_fn(|i| std::array::from_fn(|j| (p[i][j], 0.0)));
    let pt = dagger4(&pc);
    let mut k1 = mul4(&up, &pc);
    for row in &mut k1 {
        for (k, x) in row.iter_mut().enumerate() {
            *x = mul(*x, cis(-theta[k]));
        }
    }
    let bd = dagger4(&b);
    let l1 = mul4(&b, &mul4(&k1, &bd));
    let l2 = mul4(&b, &mul4(&pt, &bd));
    // N's phases in the magic basis are (a−b+c, a+b−c, −a−b−c, −a+b+c).
    let coeffs = [
        (theta[0] + theta[1]) / 2.0,
        (theta[1] + theta[3]) / 2.0,
        (theta[0] + theta[3]) / 2.0,
    ];
    Some((l1, coeffs, l2))
}

/// Each coefficient into (−π/4, π/4]: N(…, x + n·π/2, …) = N(…, x, …) · (i·PP)ⁿ, and the
/// local factor (i·PP)ⁿ joins the gates after it.
fn reduce(coeffs: [f64; 3]) -> ([f64; 3], M4) {
    let mut out = coeffs;
    let mut extra = identity4();
    for k in 0..3 {
        let mut n = (out[k] / FRAC_PI_2).round() as i64;
        out[k] -= n as f64 * FRAC_PI_2;
        if out[k] <= -FRAC_PI_4 + EPS {
            out[k] += FRAC_PI_2;
            n -= 1;
        }
        for _ in 0..n.rem_euclid(4) {
            extra = mul4(&extra, &i_pauli_pair(k));
        }
    }
    (out, extra)
}

/// The gates for N(a, b, c), each coefficient in (−π/4, π/4], in the order they run.
fn core([a, b, c]: [f64; 3]) -> Vec<Piece> {
    use Piece::{Cx, One};
    let zero = [a.abs() < EPS, b.abs() < EPS, c.abs() < EPS];
    let nonzero: Vec<usize> = (0..3).filter(|&k| !zero[k]).collect();
    let both = |m: M2| [One(0, m), One(1, m)];
    match nonzero.as_slice() {
        [] => Vec::new(),
        // A CNOT's class: N(0, 0, ±π/4) ∝ (S∓ ⊗ S∓) · CZ, and CZ = (I⊗H) · CX · (I⊗H),
        // conjugated into XX or YY.
        &[k] if ((a + b + c).abs() - FRAC_PI_4).abs() < EPS => {
            let theta = [a, b, c][k];
            let s = if theta > 0.0 {
                dagger2(&s_gate())
            } else {
                s_gate()
            };
            let to_zz = match k {
                0 => hadamard(),
                1 => mul2(&s_gate(), &hadamard()),
                _ => [[ONE, ZERO], [ZERO, ONE]],
            };
            let mut gates = both(dagger2(&to_zz)).to_vec();
            gates.extend([
                One(1, hadamard()),
                Cx(0, 1),
                One(1, hadamard()),
                One(0, s),
                One(1, s),
            ]);
            gates.extend(both(to_zz));
            gates
        }
        // One coefficient 0: CX · (Rx(−2x) ⊗ Rz(−2z)) · CX is exp(i(x·XX + z·ZZ)), conjugated
        // so the zero falls on YY.
        _ if zero.iter().any(|&z| z) => {
            let (x, z, to) = if zero[1] {
                (a, c, None)
            } else if zero[0] {
                (b, c, Some(dagger2(&s_gate())))
            } else {
                (a, b, Some(dagger2(&rx(-FRAC_PI_2))))
            };
            let mut gates = Vec::new();
            if let Some(m) = to {
                gates.extend(both(m));
            }
            gates.extend([
                Cx(0, 1),
                One(0, rx(-2.0 * x)),
                One(1, rz(-2.0 * z)),
                Cx(0, 1),
            ]);
            if let Some(m) = to {
                gates.extend(both(dagger2(&m)));
            }
            gates
        }
        // Three CNOTs (Vatan and Williams).
        _ => vec![
            One(1, rz(FRAC_PI_2)),
            Cx(1, 0),
            One(0, rz(FRAC_PI_2 - 2.0 * c)),
            One(1, ry(FRAC_PI_2 - 2.0 * a)),
            Cx(0, 1),
            One(1, ry(2.0 * b - FRAC_PI_2)),
            Cx(1, 0),
            One(0, rz(-FRAC_PI_2)),
        ],
    }
}

/// The matrix `pieces` apply, in order.
pub fn matrix(pieces: &[Piece]) -> M4 {
    let mut m = identity4();
    for piece in pieces {
        let g = match *piece {
            Piece::One(0, ref u) => kron(u, &[[ONE, ZERO], [ZERO, ONE]]),
            Piece::One(_, ref u) => kron(&[[ONE, ZERO], [ZERO, ONE]], u),
            Piece::Cx(control, _) => {
                let mut g = [[ZERO; 4]; 4];
                for i in 0..4 {
                    let flips = if control == 0 { i >> 1 & 1 } else { i & 1 } == 1;
                    let j = if flips {
                        i ^ if control == 0 { 1 } else { 2 }
                    } else {
                        i
                    };
                    g[j][i] = ONE;
                }
                g
            }
        };
        m = mul4(&g, &m);
    }
    m
}

/// Merges neighbouring one-qubit matrices on a qubit and drops those that are the identity
/// up to a phase (which the caller's overall phase absorbs).
fn merge(pieces: Vec<Piece>) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut pending: [Option<M2>; 2] = [None, None];
    let flush = |q: usize, pending: &mut [Option<M2>; 2], out: &mut Vec<Piece>| {
        if let Some(m) = pending[q].take() {
            let phase = if abs(m[0][0]) > 0.5 { m[0][0] } else { ONE };
            let identity = abs(m[0][1]) < 1e-12
                && abs(m[1][0]) < 1e-12
                && abs(add(m[0][0], scale(m[1][1], -1.0))) < 1e-12
                && abs(phase) > 0.5;
            if !identity {
                out.push(Piece::One(q, m));
            }
        }
    };
    for piece in pieces {
        match piece {
            Piece::One(q, m) => {
                pending[q] = Some(match pending[q] {
                    Some(earlier) => mul2(&m, &earlier),
                    None => m,
                });
            }
            Piece::Cx(..) => {
                flush(0, &mut pending, &mut out);
                flush(1, &mut pending, &mut out);
                out.push(piece);
            }
        }
    }
    flush(0, &mut pending, &mut out);
    flush(1, &mut pending, &mut out);
    out
}

/// `u` as the fewest CNOTs and one-qubit matrices, with the overall phase they leave out:
/// `u = e^{iφ} · matrix(pieces)`. `None` if the numbers did not settle, so the caller can
/// fall back on another decomposition.
pub fn decompose(u: &M4) -> Option<(Vec<Piece>, f64)> {
    let (l1, coeffs, l2) = split(u)?;
    let (coeffs, extra) = reduce(coeffs);
    let (a2, b2) = factor(&mul4(&extra, &l2));
    let (a1, b1) = factor(&l1);
    let mut pieces = vec![Piece::One(0, a2), Piece::One(1, b2)];
    pieces.extend(core(coeffs));
    pieces.extend([Piece::One(0, a1), Piece::One(1, b1)]);
    let pieces = merge(pieces);
    // The overall phase, from the largest entry, and a check that the rest is exact.
    let r = matrix(&pieces);
    let (i, j) = (0..16)
        .map(|k| (k / 4, k % 4))
        .max_by(|&(a, b), &(c, d)| abs(u[a][b]).total_cmp(&abs(u[c][d])))
        .expect("sixteen entries");
    let phase = arg(div(u[i][j], r[i][j]));
    let factor = cis(phase);
    let exact =
        (0..4).all(|i| (0..4).all(|j| abs(add(u[i][j], scale(mul(factor, r[i][j]), -1.0))) < 1e-9));
    exact.then_some((pieces, phase))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cnots(pieces: &[Piece]) -> usize {
        pieces.iter().filter(|p| matches!(p, Piece::Cx(..))).count()
    }

    fn gate(entries: [[C; 4]; 4]) -> M4 {
        entries
    }

    /// A unitary from a seed: Gram–Schmidt on a pseudo-random complex matrix.
    fn random_unitary(seed: u64) -> M4 {
        let mut state = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
        };
        let mut cols: Vec<[C; 4]> = (0..4)
            .map(|_| std::array::from_fn(|_| (next(), next())))
            .collect();
        for k in 0..4 {
            for j in 0..k {
                let dot = (0..4).fold(ZERO, |acc, i| add(acc, mul(conj(cols[j][i]), cols[k][i])));
                for i in 0..4 {
                    cols[k][i] = add(cols[k][i], scale(mul(dot, cols[j][i]), -1.0));
                }
            }
            let n = cols[k].iter().map(|&x| abs(x) * abs(x)).sum::<f64>().sqrt();
            for x in &mut cols[k] {
                *x = scale(*x, 1.0 / n);
            }
        }
        std::array::from_fn(|i| std::array::from_fn(|j| cols[j][i]))
    }

    #[test]
    fn known_gates_take_the_fewest_cnots() {
        let (o, z, i) = (ONE, ZERO, (0.0, 1.0));
        let cnot = gate([[o, z, z, z], [z, o, z, z], [z, z, z, o], [z, z, o, z]]);
        let reversed = gate([[o, z, z, z], [z, z, z, o], [z, z, o, z], [z, o, z, z]]);
        let cz = gate([
            [o, z, z, z],
            [z, o, z, z],
            [z, z, o, z],
            [z, z, z, (-1.0, 0.0)],
        ]);
        let swap = gate([[o, z, z, z], [z, z, o, z], [z, o, z, z], [z, z, z, o]]);
        let iswap = gate([[o, z, z, z], [z, z, i, z], [z, i, z, z], [z, z, z, o]]);
        let local = kron(&hadamard(), &rz(0.3));
        for (name, u, want) in [
            ("identity", identity4(), 0),
            ("local", local, 0),
            ("cnot", cnot, 1),
            ("reversed cnot", reversed, 1),
            ("cz", cz, 1),
            ("iswap", iswap, 2),
            ("swap", swap, 3),
        ] {
            let (pieces, _) = decompose(&u).unwrap_or_else(|| panic!("{name} did not settle"));
            assert_eq!(cnots(&pieces), want, "{name}");
        }
        for seed in 0..50 {
            let (pieces, _) = decompose(&random_unitary(seed)).expect("settles");
            assert_eq!(cnots(&pieces), 3, "random {seed}");
        }
    }
}
