//! Gates given as matrices (`spec/core.md` §1). A matrix travels as its gate's parameters,
//! the real and imaginary part of each entry, rows first: 8 numbers for one qubit, 32 for two.

use crate::value::Value;

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
