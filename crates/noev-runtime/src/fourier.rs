//! `fft` and `ifft` (`spec/std.md` §3): the discrete Fourier transform, as NumPy computes it.
//! Forward, X[k] = Σ x[n] e^(−2πi·kn/N); the inverse has the opposite sign and divides by N.

use std::f64::consts::TAU;

/// The transform of `x`, as (real, imaginary) parts: radix-2 for a power-of-two length,
/// otherwise the sum term by term.
pub fn transform(x: &[(f64, f64)], inverse: bool) -> Vec<(f64, f64)> {
    let sign = if inverse { 1.0 } else { -1.0 };
    let mut out = if x.len().is_power_of_two() {
        radix2(x, sign)
    } else {
        direct(x, sign)
    };
    if inverse {
        let n = x.len() as f64;
        for (re, im) in &mut out {
            *re /= n;
            *im /= n;
        }
    }
    out
}

/// e^(sign·2πi·k/n).
fn twiddle(k: usize, n: usize, sign: f64) -> (f64, f64) {
    let angle = sign * TAU * k as f64 / n as f64;
    (angle.cos(), angle.sin())
}

fn mul((a, b): (f64, f64), (c, d): (f64, f64)) -> (f64, f64) {
    (a * c - b * d, a * d + b * c)
}

fn direct(x: &[(f64, f64)], sign: f64) -> Vec<(f64, f64)> {
    let n = x.len();
    (0..n)
        .map(|k| {
            x.iter().enumerate().fold((0.0, 0.0), |(re, im), (j, &v)| {
                // k·j can be large; only it modulo n decides the twiddle.
                let (a, b) = mul(v, twiddle(k * j % n, n, sign));
                (re + a, im + b)
            })
        })
        .collect()
}

/// Iterative Cooley–Tukey: the input in bit-reversed order, then log₂ n passes of butterflies.
fn radix2(x: &[(f64, f64)], sign: f64) -> Vec<(f64, f64)> {
    let n = x.len();
    let bits = n.trailing_zeros();
    let mut a: Vec<(f64, f64)> = (0..n)
        .map(|i| {
            x[if bits == 0 {
                0
            } else {
                i.reverse_bits() >> (usize::BITS - bits)
            }]
        })
        .collect();
    let mut size = 2;
    while size <= n {
        let half = size / 2;
        for start in (0..n).step_by(size) {
            for k in 0..half {
                let t = mul(a[start + k + half], twiddle(k, size, sign));
                let u = a[start + k];
                a[start + k] = (u.0 + t.0, u.1 + t.1);
                a[start + k + half] = (u.0 - t.0, u.1 - t.1);
            }
        }
        size *= 2;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: &[(f64, f64)], b: &[(f64, f64)]) -> bool {
        a.iter()
            .zip(b)
            .all(|(x, y)| (x.0 - y.0).abs() < 1e-12 && (x.1 - y.1).abs() < 1e-12)
    }

    #[test]
    fn matches_the_definition_and_inverts() {
        let x: Vec<(f64, f64)> = (0..8).map(|i| (i as f64, (i * i) as f64 / 7.0)).collect();
        // Radix-2 and the direct sum agree on a power of two.
        assert!(close(&radix2(&x, -1.0), &direct(&x, -1.0)));
        for len in [1, 2, 3, 5, 8] {
            let x = &x[..len];
            assert!(
                close(&transform(&transform(x, false), true), x),
                "length {len}"
            );
        }
        // A constant is all frequency 0.
        let ones = vec![(1.0, 0.0); 4];
        assert!(close(
            &transform(&ones, false),
            &[(4.0, 0.0), (0.0, 0.0), (0.0, 0.0), (0.0, 0.0)]
        ));
        // e^(2πi·n/4) is all frequency 1.
        let wave: Vec<(f64, f64)> = (0..4).map(|n| twiddle(n, 4, 1.0)).collect();
        assert!(close(
            &transform(&wave, false),
            &[(0.0, 0.0), (4.0, 0.0), (0.0, 0.0), (0.0, 0.0)]
        ));
    }
}
