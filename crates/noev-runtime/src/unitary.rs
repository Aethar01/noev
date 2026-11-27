//! Gates given as matrices (`spec/core.md` §1). A matrix travels as its gate's parameters,
//! the real and imaginary part of each entry, rows first: `2 * 4^N` numbers for `N` qubits
//! (8 for 1 qubit, 32 for 2, 128 for 3, 512 for 4, …).

use noev_sema::prelude::Gate;

use crate::backend::Op;
use crate::value::Value;

/// A complex number as (real, imaginary) parts.
type C = (f64, f64);

/// A square complex matrix, stored row-major.
type Mat = Vec<Vec<C>>;

fn mul((a, b): C, (c, d): C) -> C {
    (a * c - b * d, a * d + b * c)
}

fn add((a, b): C, (c, d): C) -> C {
    (a + c, b + d)
}

fn sub((a, b): C, (c, d): C) -> C {
    (a - c, b - d)
}

fn conj((a, b): C) -> C {
    (a, -b)
}

fn scale((a, b): C, s: f64) -> C {
    (a * s, b * s)
}

fn norm2((a, b): C) -> f64 {
    a * a + b * b
}

fn abs((a, b): C) -> f64 {
    a.hypot(b)
}

fn arg((a, b): C) -> f64 {
    b.atan2(a)
}

fn exp_i(theta: f64) -> C {
    (theta.cos(), theta.sin())
}

/// How close `m·m†` must be to the identity, entry by entry.
const TOLERANCE: f64 = 1e-9;

/// The side `2^N` of a square matrix with these parameters (`params.len() == 2 * side * side`).
pub fn size(params: &[f64]) -> usize {
    let entries = params.len() / 2;
    let side = (entries as f64).sqrt().round() as usize;
    if side >= 2 && side.is_power_of_two() && 2 * side * side == params.len() {
        side
    } else {
        2
    }
}

/// The number of qubits `N` for a `2^N × 2^N` matrix with these parameters.
pub fn qubits(params: &[f64]) -> usize {
    size(params).trailing_zeros() as usize
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
/// backend and OpenQASM have: the fewest CNOTs, between one-qubit matrices
/// ([`crate::kak`]). Should that not settle numerically, [`rotations`] gives it exactly
/// with more two-qubit gates. Either way the overall phase is kept, as `p·x·p·x`, since an
/// outer `ctrl` makes it a relative one.
pub fn decompose(params: &[f64], adjoint: bool, first: usize, second: usize) -> Vec<Op> {
    let m = rows(params, adjoint);
    let matrix: [[C; 4]; 4] = std::array::from_fn(|i| std::array::from_fn(|j| m[i][j]));
    let Some((pieces, phase)) = crate::kak::decompose(&matrix) else {
        return rotations(params, adjoint, first, second);
    };
    let qubit = |q: usize| if q == 0 { first } else { second };
    let mut ops = Vec::new();
    if phase.abs() > 1e-15 {
        // e^{iφ}·I = X · P(φ) · X · P(φ).
        for _ in 0..2 {
            ops.push(op(Gate::P, vec![phase], vec![], first));
            ops.push(op(Gate::X, vec![], vec![], first));
        }
    }
    for piece in pieces {
        ops.push(match piece {
            crate::kak::Piece::One(q, u) => {
                let params = u.iter().flatten().flat_map(|&(re, im)| [re, im]).collect();
                op(Gate::Unitary, params, vec![], qubit(q))
            }
            crate::kak::Piece::Cx(control, target) => {
                op(Gate::X, vec![], vec![qubit(control)], qubit(target))
            }
        });
    }
    ops
}

/// A two-qubit matrix as rotations of pairs of basis states: in the basis order [`CHAIN`],
/// rotations of neighbouring rows (Givens) reduce the matrix to a diagonal; each rotation
/// acts on two basis states that differ in one qubit, so it is a one-qubit gate controlled by
/// the other, and the diagonal is phase gates. Always exact, but up to six controlled gates.
fn rotations(params: &[f64], adjoint: bool, first: usize, second: usize) -> Vec<Op> {
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

// ---------------------------------------------------------------------------------------
// N-qubit unitary decomposition (N >= 1) via Quantum Shannon Decomposition (QSD)
// ---------------------------------------------------------------------------------------

/// Decomposes an `N`-qubit unitary matrix on `targets` (`targets[0]` most significant) into
/// single-qubit gates (`Unitary`, `Ry`, `Rz`, `P`, `X`) and CNOTs (`X` with one control),
/// preserving exact global phase so outer `ctrl` works without adjustment.
///
/// - `N = 1`: returns a single 1-qubit `Gate::Unitary`.
/// - `N = 2`: optimal KAK decomposition (at most 3 CNOTs; [`decompose`]).
/// - `N >= 3`: recursive Quantum Shannon Decomposition (Cosine–Sine Decomposition +
///   demultiplexing + Walsh–Hadamard Gray-code uniformly controlled rotations), bottoming out
///   in the optimal 2-qubit KAK decomposition.
pub fn decompose_n(params: &[f64], adjoint: bool, targets: &[usize]) -> Vec<Op> {
    match targets {
        [] => Vec::new(),
        &[q] => {
            let m = rows(params, adjoint);
            vec![op(Gate::Unitary, flatten_mat(&m), vec![], q)]
        }
        &[first, second] => decompose(params, adjoint, first, second),
        _ => {
            let m = rows(params, adjoint);
            let mut ops = Vec::new();
            qsd(&m, targets, &mut ops);
            ops
        }
    }
}

fn flatten_mat(m: &Mat) -> Vec<f64> {
    let mut out = Vec::with_capacity(2 * m.len() * m.len());
    for row in m {
        for &(re, im) in row {
            out.push(re);
            out.push(im);
        }
    }
    out
}

fn mat_identity(n: usize) -> Mat {
    let mut m = vec![vec![(0.0, 0.0); n]; n];
    for (i, row) in m.iter_mut().enumerate() {
        row[i] = (1.0, 0.0);
    }
    m
}

fn mat_dagger(a: &Mat) -> Mat {
    let n = a.len();
    let mut out = vec![vec![(0.0, 0.0); n]; n];
    for i in 0..n {
        for j in 0..n {
            out[i][j] = conj(a[j][i]);
        }
    }
    out
}

fn mat_mul(a: &Mat, b: &Mat) -> Mat {
    let n = a.len();
    let mut out = vec![vec![(0.0, 0.0); n]; n];
    for i in 0..n {
        for k in 0..n {
            let aik = a[i][k];
            if aik == (0.0, 0.0) {
                continue;
            }
            for j in 0..n {
                out[i][j] = add(out[i][j], mul(aik, b[k][j]));
            }
        }
    }
    out
}

fn is_identity_mat(m: &Mat) -> bool {
    let n = m.len();
    for (i, row) in m.iter().enumerate() {
        for (j, &(re, im)) in row.iter().enumerate() {
            let want = if i == j { 1.0 } else { 0.0 };
            if (re - want).abs() > 1e-13 || im.abs() > 1e-13 {
                return false;
            }
        }
    }
    let _ = n;
    true
}

fn mats_close(a: &Mat, b: &Mat) -> bool {
    for (ra, rb) in a.iter().zip(b) {
        for (&x, &y) in ra.iter().zip(rb) {
            if abs(sub(x, y)) > 1e-13 {
                return false;
            }
        }
    }
    true
}

/// Complex Hermitian Jacobi eigendecomposition: returns `(evals, Q)` with `Q` unitary and
/// `Q† · H · Q = diag(evals)`.
fn hermitian_jacobi(h: &Mat) -> (Vec<f64>, Mat) {
    let n = h.len();
    let mut a = h.clone();
    let mut q = mat_identity(n);
    // Symmetrise to exact Hermiticity.
    for i in 0..n {
        a[i][i].1 = 0.0;
        for j in (i + 1)..n {
            let avg = scale(add(a[i][j], conj(a[j][i])), 0.5);
            a[i][j] = avg;
            a[j][i] = conj(avg);
        }
    }
    let norm = a
        .iter()
        .flatten()
        .map(|&z| norm2(z))
        .sum::<f64>()
        .sqrt()
        .max(1.0);
    for _ in 0..100 {
        let mut max_off = 0.0f64;
        for (p, row) in a.iter().enumerate() {
            for &entry in &row[(p + 1)..] {
                let mag = abs(entry);
                if mag > max_off {
                    max_off = mag;
                }
            }
        }
        if max_off <= 1e-15 * norm {
            break;
        }
        for p in 0..n {
            for r in (p + 1)..n {
                let apr = a[p][r];
                let mag = abs(apr);
                if mag <= 1e-16 * (a[p][p].0.abs() + a[r][r].0.abs() + 1e-30) {
                    continue;
                }
                let ep = scale(apr, 1.0 / mag);
                let em = conj(ep);
                let tau = (a[r][r].0 - a[p][p].0) / (2.0 * mag);
                let t = if tau >= 0.0 {
                    -1.0 / (tau + (1.0 + tau * tau).sqrt())
                } else {
                    1.0 / (-tau + (1.0 + tau * tau).sqrt())
                };
                let c = 1.0 / (1.0 + t * t).sqrt();
                let s = t * c;
                let s_ep = scale(ep, s);
                let s_em = scale(em, s);
                // Right-multiply A by G: update columns p and r.
                for row in &mut a {
                    let ap = row[p];
                    let ar = row[r];
                    row[p] = add(scale(ap, c), mul(s_em, ar));
                    row[r] = add(mul((-s_ep.0, -s_ep.1), ap), scale(ar, c));
                }
                // Left-multiply A by G†: update rows p and r.
                for j in 0..n {
                    let ap = a[p][j];
                    let ar = a[r][j];
                    a[p][j] = add(scale(ap, c), mul(s_ep, ar));
                    a[r][j] = add(mul((-s_em.0, -s_em.1), ap), scale(ar, c));
                }
                a[p][r] = (0.0, 0.0);
                a[r][p] = (0.0, 0.0);
                a[p][p].1 = 0.0;
                a[r][r].1 = 0.0;
                // Accumulate eigenvectors: Q = Q * G.
                for row in &mut q {
                    let qp = row[p];
                    let qr = row[r];
                    row[p] = add(scale(qp, c), mul(s_em, qr));
                    row[r] = add(mul((-s_ep.0, -s_ep.1), qp), scale(qr, c));
                }
            }
        }
    }
    let evals = (0..n).map(|i| a[i][i].0).collect();
    (evals, q)
}

/// Eigendecomposition of a unitary matrix `M`: returns `(phases, W)` such that `W` is unitary
/// and `W† · M · W = diag(e^{i·phases[k]})`.
///
/// Decomposes `M = H₁ + i H₂` into commuting Hermitian matrices, diagonalises `H₁`, then
/// diagonalises `H₂` within every degenerate eigenspace of `H₁`.
fn unitary_eig(m: &Mat) -> (Vec<f64>, Mat) {
    let n = m.len();
    let m_dag = mat_dagger(m);
    let mut h1 = vec![vec![(0.0, 0.0); n]; n];
    for i in 0..n {
        for j in 0..n {
            h1[i][j] = scale(add(m[i][j], m_dag[i][j]), 0.5);
        }
    }
    let (evals1, q1_raw) = hermitian_jacobi(&h1);
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| evals1[a].total_cmp(&evals1[b]));
    let sorted_e1: Vec<f64> = order.iter().map(|&i| evals1[i]).collect();
    let mut w = vec![vec![(0.0, 0.0); n]; n];
    for i in 0..n {
        for (col, &src) in order.iter().enumerate() {
            w[i][col] = q1_raw[i][src];
        }
    }
    // Transform M into Q₁'s basis and resolve degenerate eigenspaces of H₁ using H₂.
    let m_prime = mat_mul(&mat_dagger(&w), &mat_mul(m, &w));
    let mut start = 0;
    while start < n {
        let mut end = start + 1;
        while end < n && (sorted_e1[end] - sorted_e1[start]).abs() < 1e-6 {
            end += 1;
        }
        let k = end - start;
        if k > 1 {
            let mut h2_sub = vec![vec![(0.0, 0.0); k]; k];
            for i in 0..k {
                for j in 0..k {
                    // (M' - M'†) / (2i): dividing (x, y) by 2i gives (y/2, -x/2).
                    let diff = sub(
                        m_prime[start + i][start + j],
                        conj(m_prime[start + j][start + i]),
                    );
                    h2_sub[i][j] = (diff.1 * 0.5, -diff.0 * 0.5);
                }
            }
            let (_, q_sub) = hermitian_jacobi(&h2_sub);
            // Multiply columns start..end of W on the right by q_sub.
            for row in &mut w {
                let orig: Vec<C> = (0..k).map(|j| row[start + j]).collect();
                for j in 0..k {
                    let mut sum = (0.0, 0.0);
                    for l in 0..k {
                        sum = add(sum, mul(orig[l], q_sub[l][j]));
                    }
                    row[start + j] = sum;
                }
            }
        }
        start = end;
    }
    // Compute exact eigenphases from diagonal of W† · M · W.
    let diag_m = mat_mul(&mat_dagger(&w), &mat_mul(m, &w));
    let phases = (0..n).map(|i| arg(diag_m[i][i])).collect();
    (phases, w)
}

/// Orthonormalises a column vector `vec` against `basis` (two-pass Modified Gram-Schmidt),
/// returning `Some(unit_vec)` if its remaining norm exceeds `1e-10`.
fn orthogonalise(vec: &[C], basis: &[Vec<C>]) -> Option<Vec<C>> {
    let mut v = vec.to_vec();
    for _ in 0..2 {
        for b in basis {
            let mut dot = (0.0, 0.0);
            for (&bi, &vi) in b.iter().zip(&v) {
                dot = add(dot, mul(conj(bi), vi));
            }
            for (vi, &bi) in v.iter_mut().zip(b) {
                *vi = sub(*vi, mul(dot, bi));
            }
        }
    }
    let norm = v.iter().map(|&z| norm2(z)).sum::<f64>().sqrt();
    if norm < 1e-10 {
        return None;
    }
    for z in &mut v {
        *z = scale(*z, 1.0 / norm);
    }
    Some(v)
}

/// Completes a partially filled set of orthonormal columns (`None` entries are missing) into
/// a full `m × m` unitary matrix.
fn complete_columns(mut cols: Vec<Option<Vec<C>>>, m: usize) -> Mat {
    let mut basis: Vec<Vec<C>> = cols.iter().filter_map(|c| c.clone()).collect();
    let mut next_e = 0usize;
    for slot in &mut cols {
        if slot.is_some() {
            continue;
        }
        while next_e < m {
            let mut e = vec![(0.0, 0.0); m];
            e[next_e] = (1.0, 0.0);
            next_e += 1;
            if let Some(u) = orthogonalise(&e, &basis) {
                basis.push(u.clone());
                *slot = Some(u);
                break;
            }
        }
    }
    let mut mat = vec![vec![(0.0, 0.0); m]; m];
    for (j, col) in cols.into_iter().enumerate() {
        let col = col.expect("orthonormal completion always spans C^m");
        for i in 0..m {
            mat[i][j] = col[i];
        }
    }
    mat
}

/// Cosine–Sine Decomposition of a `2m × 2m` unitary matrix `U`:
/// `U = (A₁ ⊕ A₂) · [[C, -S], [S, C]] · (B₁ ⊕ B₂)` with `C = diag(cos θ_k)`,
/// `S = diag(sin θ_k)`, `θ_k ∈ [0, π/2]`.
fn csd(u: &Mat) -> (Mat, Mat, Vec<f64>, Mat, Mat) {
    let n = u.len();
    let m = n / 2;
    let block = |r0: usize, c0: usize| -> Mat {
        (0..m)
            .map(|i| (0..m).map(|j| u[r0 + i][c0 + j]).collect())
            .collect()
    };
    let u00 = block(0, 0);
    let u01 = block(0, m);
    let u10 = block(m, 0);
    let u11 = block(m, m);

    // SVD of U₀₀: U₀₀† · U₀₀ = V · C² · V†, sorted with cosines descending.
    let g = mat_mul(&mat_dagger(&u00), &u00);
    let (lam, v_raw) = hermitian_jacobi(&g);
    let mut order: Vec<usize> = (0..m).collect();
    order.sort_by(|&a, &b| lam[b].total_cmp(&lam[a]));
    let mut v = vec![vec![(0.0, 0.0); m]; m];
    let mut c_vals = Vec::with_capacity(m);
    let mut s_vals = Vec::with_capacity(m);
    let mut thetas = Vec::with_capacity(m);
    for (col, &src) in order.iter().enumerate() {
        for i in 0..m {
            v[i][col] = v_raw[i][src];
        }
        let c = lam[src].clamp(0.0, 1.0).sqrt();
        let s = (1.0 - c * c).max(0.0).sqrt();
        c_vals.push(c);
        s_vals.push(s);
        thetas.push(s.atan2(c));
    }
    let b1 = mat_dagger(&v);

    // Columns of P = U₀₀·V give A₁ (largest cosines first).
    let p = mat_mul(&u00, &v);
    let mut a1_cols: Vec<Option<Vec<C>>> = vec![None; m];
    let mut a1_basis: Vec<Vec<C>> = Vec::with_capacity(m);
    for (k, &ck) in c_vals.iter().enumerate() {
        if ck > 1e-11 {
            let raw: Vec<C> = (0..m).map(|i| scale(p[i][k], 1.0 / ck)).collect();
            let col = if ck < 1e-3 {
                orthogonalise(&raw, &a1_basis).unwrap_or(raw)
            } else {
                raw
            };
            a1_basis.push(col.clone());
            a1_cols[k] = Some(col);
        }
    }
    let a1 = complete_columns(a1_cols, m);

    // Columns of Q = U₁₀·V give A₂ (largest sines first, i.e. reverse k order).
    let q = mat_mul(&u10, &v);
    let mut a2_cols: Vec<Option<Vec<C>>> = vec![None; m];
    let mut a2_basis: Vec<Vec<C>> = Vec::with_capacity(m);
    for k in (0..m).rev() {
        let sk = s_vals[k];
        if sk > 1e-11 {
            let raw: Vec<C> = (0..m).map(|i| scale(q[i][k], 1.0 / sk)).collect();
            let col = if sk < 1e-3 {
                orthogonalise(&raw, &a2_basis).unwrap_or(raw)
            } else {
                raw
            };
            a2_basis.push(col.clone());
            a2_cols[k] = Some(col);
        }
    }
    let a2 = complete_columns(a2_cols, m);

    // B₂ satisfies A₁†·U₀₁ = -S·B₂ and A₂†·U₁₁ = C·B₂; pick the row with max(c_k, s_k) >= 1/√2.
    let x = mat_mul(&mat_dagger(&a1), &u01);
    let y = mat_mul(&mat_dagger(&a2), &u11);
    let mut b2 = vec![vec![(0.0, 0.0); m]; m];
    for k in 0..m {
        if c_vals[k] >= s_vals[k] {
            let inv = 1.0 / c_vals[k];
            for j in 0..m {
                b2[k][j] = scale(y[k][j], inv);
            }
        } else {
            let inv = -1.0 / s_vals[k];
            for j in 0..m {
                b2[k][j] = scale(x[k][j], inv);
            }
        }
    }

    (a1, a2, thetas, b1, b2)
}

/// Emits a uniformly controlled `Ry` or `Rz` rotation on `target` controlled by `controls`
/// (`controls[0]` most significant): for basis state `|k⟩` of `controls`, applies
/// `gate(angles[k])` to `target`, using Walsh–Hadamard + reflected Gray code (`<= 2^m` CNOTs).
fn emit_ucr(gate: Gate, angles: &[f64], controls: &[usize], target: usize, ops: &mut Vec<Op>) {
    let len = angles.len();
    debug_assert_eq!(len, 1usize << controls.len());
    if angles.iter().all(|a| a.abs() < 1e-14) {
        return;
    }
    // Fast in-place Walsh–Hadamard transform.
    let mut buf = angles.to_vec();
    let mut step = 1usize;
    while step < len {
        let mut i = 0;
        while i < len {
            for j in 0..step {
                let u = buf[i + j];
                let v = buf[i + j + step];
                buf[i + j] = u + v;
                buf[i + j + step] = u - v;
            }
            i += 2 * step;
        }
        step <<= 1;
    }
    let inv_len = 1.0 / (len as f64);
    let m = controls.len();
    // Adjacent CNOTs on the same target commute and self-cancel; track pending ones as a set.
    let mut pending_cnots: Vec<usize> = Vec::new();
    for j in 0..len {
        let gray = j ^ (j >> 1);
        let theta = buf[gray] * inv_len;
        if theta.abs() > 1e-14 {
            for c in pending_cnots.drain(..) {
                ops.push(op(Gate::X, vec![], vec![c], target));
            }
            ops.push(op(gate, vec![theta], vec![], target));
        }
        let next = (j + 1) & (len - 1);
        let next_gray = next ^ (next >> 1);
        let diff_bit = (gray ^ next_gray).trailing_zeros() as usize;
        let ctrl_q = controls[m - 1 - diff_bit];
        if let Some(pos) = pending_cnots.iter().position(|&c| c == ctrl_q) {
            pending_cnots.remove(pos);
        } else {
            pending_cnots.push(ctrl_q);
        }
    }
    for c in pending_cnots {
        ops.push(op(Gate::X, vec![], vec![c], target));
    }
}

/// Demultiplexes a block-diagonal unitary `V₁ ⊕ V₂` on `targets`:
/// `V₁ ⊕ V₂ = (I ⊗ W) · UCRz(-2φ) · (I ⊗ W')`.
fn emit_demux(v1: &Mat, v2: &Mat, targets: &[usize], ops: &mut Vec<Op>) {
    let rest = &targets[1..];
    if mats_close(v1, v2) {
        qsd(v1, rest, ops);
        return;
    }
    let m_mat = mat_mul(v1, &mat_dagger(v2));
    let (phases, w) = unitary_eig(&m_mat);
    let phis: Vec<f64> = phases.iter().map(|&p| 0.5 * p).collect();
    // W' = D† · W† · V₁, where D = diag(e^{i·φ_k}).
    let mut w_prime = mat_mul(&mat_dagger(&w), v1);
    for (k, row) in w_prime.iter_mut().enumerate() {
        let d_dag = exp_i(-phis[k]);
        for entry in row.iter_mut() {
            *entry = mul(d_dag, *entry);
        }
    }
    qsd(&w_prime, rest, ops);
    let rz_angles: Vec<f64> = phis.iter().map(|&p| -2.0 * p).collect();
    emit_ucr(Gate::Rz, &rz_angles, rest, targets[0], ops);
    qsd(&w, rest, ops);
}

/// Recursive Quantum Shannon Decomposition of a `2^N × 2^N` unitary matrix `u` on `targets`.
fn qsd(u: &Mat, targets: &[usize], ops: &mut Vec<Op>) {
    if is_identity_mat(u) {
        return;
    }
    match targets {
        [] => {}
        &[q] => ops.push(op(Gate::Unitary, flatten_mat(u), vec![], q)),
        &[first, second] => ops.extend(decompose(&flatten_mat(u), false, first, second)),
        _ => {
            let (a1, a2, thetas, b1, b2) = csd(u);
            emit_demux(&b1, &b2, targets, ops);
            let ry_angles: Vec<f64> = thetas.iter().map(|&t| 2.0 * t).collect();
            emit_ucr(Gate::Ry, &ry_angles, &targets[1..], targets[0], ops);
            emit_demux(&a1, &a2, targets, ops);
        }
    }
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
                // Both decompositions: the fewest CNOTs, and the rotations it falls back on.
                let got = apply(&decompose(&params, adjoint, 0, 1));
                let fallback = apply(&rotations(&params, adjoint, 0, 1));
                let want = rows(&params, adjoint);
                for (got_row, want_row) in fallback.iter().zip(&want) {
                    for (g, w) in got_row.iter().zip(want_row) {
                        assert!((g.0 - w.0).abs() < 1e-12 && (g.1 - w.1).abs() < 1e-12);
                    }
                }
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

    /// Simulates a list of 1-qubit / controlled-1-qubit `Op`s on `n` qubits (`0` = MSB)
    /// and returns the exact `2^n × 2^n` unitary matrix.
    fn apply_n(n: usize, ops: &[Op]) -> Mat {
        let dim = 1usize << n;
        let bit = |index: usize, q: usize| (index >> (n - 1 - q)) & 1;
        let mut out = vec![vec![(0.0, 0.0); dim]; dim];
        for j in 0..dim {
            let mut state = vec![(0.0, 0.0); dim];
            state[j] = (1.0, 0.0);
            for op in ops {
                let target = op.targets[0];
                let m = match op.gate {
                    Gate::X => [[(0.0, 0.0), (1.0, 0.0)], [(1.0, 0.0), (0.0, 0.0)]],
                    Gate::P => [
                        [(1.0, 0.0), (0.0, 0.0)],
                        [(0.0, 0.0), (op.params[0].cos(), op.params[0].sin())],
                    ],
                    Gate::Ry => {
                        let c = (op.params[0] * 0.5).cos();
                        let s = (op.params[0] * 0.5).sin();
                        [[(c, 0.0), (-s, 0.0)], [(s, 0.0), (c, 0.0)]]
                    }
                    Gate::Rz => {
                        let a = op.params[0] * 0.5;
                        [
                            [(a.cos(), -a.sin()), (0.0, 0.0)],
                            [(0.0, 0.0), (a.cos(), a.sin())],
                        ]
                    }
                    Gate::Unitary => {
                        let r = rows(&op.params, op.adjoint);
                        [[r[0][0], r[0][1]], [r[1][0], r[1][1]]]
                    }
                    other => panic!("unexpected gate {other:?} in decompose_n"),
                };
                let step = 1usize << (n - 1 - target);
                let mut next = state.clone();
                for (i, slot) in next.iter_mut().enumerate() {
                    if op.controls.iter().any(|&c| bit(i, c) == 0) {
                        continue;
                    }
                    let partner = i ^ step;
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
            for i in 0..dim {
                out[i][j] = state[i];
            }
        }
        out
    }

    /// Generates a deterministic pseudo-random Haar unitary of dimension `dim` via Gram-Schmidt.
    fn random_unitary_n(dim: usize, mut seed: u64) -> Mat {
        let mut next = || -> f64 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 11) as f64) / ((1u64 << 53) as f64) * 2.0 - 1.0
        };
        let mut cols: Vec<Vec<C>> = Vec::with_capacity(dim);
        for _ in 0..dim {
            let raw: Vec<C> = (0..dim).map(|_| (next(), next())).collect();
            let col = orthogonalise(&raw, &cols).expect("independent random column");
            cols.push(col);
        }
        let mut mat = vec![vec![(0.0, 0.0); dim]; dim];
        for i in 0..dim {
            for j in 0..dim {
                mat[i][j] = cols[j][i];
            }
        }
        mat
    }

    fn qft_matrix(n: usize) -> Mat {
        let dim = 1usize << n;
        let inv = 1.0 / (dim as f64).sqrt();
        let mut m = vec![vec![(0.0, 0.0); dim]; dim];
        for (i, row) in m.iter_mut().enumerate() {
            for (j, slot) in row.iter_mut().enumerate() {
                let angle = std::f64::consts::TAU * ((i * j) as f64) / (dim as f64);
                *slot = scale(exp_i(angle), inv);
            }
        }
        m
    }

    fn check_decompose_n(name: &str, n: usize, mat: &Mat, max_cnots: usize) {
        let dim = 1usize << n;
        let flat = flatten_mat(mat);
        assert_eq!(size(&flat), dim);
        assert_eq!(qubits(&flat), n);
        let targets: Vec<usize> = (0..n).collect();
        for adjoint in [false, true] {
            let ops = decompose_n(&flat, adjoint, &targets);
            let cnots = ops
                .iter()
                .filter(|o| o.gate == Gate::X && !o.controls.is_empty())
                .count();
            assert!(
                cnots <= max_cnots,
                "{name} (adj={adjoint}): used {cnots} CNOTs, expected <= {max_cnots}"
            );
            for o in &ops {
                assert!(
                    o.controls.len() <= 1,
                    "{name}: expected only 1-qubit gates and CNOTs, got {:?}",
                    o
                );
            }
            let got = apply_n(n, &ops);
            let want = rows(&flat, adjoint);
            for r in 0..dim {
                for c in 0..dim {
                    let err = abs(sub(got[r][c], want[r][c]));
                    assert!(
                        err < 1e-10,
                        "{name} (n={n}, adj={adjoint}) at ({r},{c}): got {:?}, want {:?} (err={err:.3e})",
                        got[r][c],
                        want[r][c]
                    );
                }
            }
        }
    }

    #[test]
    fn three_qubit_matrices_decompose_exactly() {
        // 1. Toffoli (CCX)
        let mut ccx = mat_identity(8);
        ccx.swap(6, 7);
        check_decompose_n("ccx", 3, &ccx, 24);

        // 2. Fredkin (CSWAP)
        let mut cswap = mat_identity(8);
        cswap.swap(5, 6);
        check_decompose_n("cswap", 3, &cswap, 24);

        // 3. 3-qubit QFT
        check_decompose_n("qft3", 3, &qft_matrix(3), 24);

        // 4. Global phase * identity (exercises exact global phase preservation)
        let mut phase_id = mat_identity(8);
        let ph = exp_i(0.73);
        for (i, row) in phase_id.iter_mut().enumerate() {
            row[i] = ph;
        }
        check_decompose_n("phase_id3", 3, &phase_id, 24);

        // 5. Block-diagonal (I ⊗ SWAP)
        let mut i_swap = mat_identity(8);
        i_swap.swap(1, 2);
        i_swap.swap(5, 6);
        check_decompose_n("i_swap", 3, &i_swap, 24);

        // 6. Anti-block-diagonal (X ⊗ I ⊗ I, pure c=0, s=1 CSD edge case)
        let mut x_ii = vec![vec![(0.0, 0.0); 8]; 8];
        for i in 0..4 {
            x_ii[i][i + 4] = (1.0, 0.0);
            x_ii[i + 4][i] = (1.0, 0.0);
        }
        check_decompose_n("x_ii", 3, &x_ii, 24);

        // 7. Diagonal with arbitrary phases
        let mut diag = vec![vec![(0.0, 0.0); 8]; 8];
        for (i, row) in diag.iter_mut().enumerate() {
            row[i] = exp_i(0.37 * (i as f64 + 1.0));
        }
        check_decompose_n("diag3", 3, &diag, 24);

        // 8. 15 random Haar 3-qubit unitaries
        for seed in 1u64..=15 {
            let u = random_unitary_n(8, seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            check_decompose_n(&format!("haar3_{seed}"), 3, &u, 24);
        }
    }

    #[test]
    fn four_qubit_matrices_decompose_exactly() {
        // 1. 4-qubit Toffoli (CCCX)
        let mut cccx = mat_identity(16);
        cccx.swap(14, 15);
        check_decompose_n("cccx", 4, &cccx, 120);

        // 2. 4-qubit QFT
        check_decompose_n("qft4", 4, &qft_matrix(4), 120);

        // 3. 3 random Haar 4-qubit unitaries
        for seed in 1u64..=3 {
            let u = random_unitary_n(16, seed.wrapping_mul(0xBF58_476D_1CE4_E5B9));
            check_decompose_n(&format!("haar4_{seed}"), 4, &u, 120);
        }
    }
}
