//! Throughput benchmark for N-qubit unitary matrix decomposition (`spec/core.md` §1).
//! Run with `cargo bench -p noev-runtime --bench unitary`.

use std::hint::black_box;
use std::time::{Duration, Instant};

use noev_runtime::unitary::decompose_n;

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

fn random_unitary_params(dim: usize, mut seed: u64) -> Vec<f64> {
    let mut next = || -> f64 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((seed >> 11) as f64) / ((1u64 << 53) as f64) * 2.0 - 1.0
    };
    let mut cols: Vec<Vec<(f64, f64)>> = Vec::with_capacity(dim);
    for _ in 0..dim {
        let mut v: Vec<(f64, f64)> = (0..dim).map(|_| (next(), next())).collect();
        for _ in 0..2 {
            for b in &cols {
                let mut dot = (0.0, 0.0);
                for (&(ar, ai), &(vr, vi)) in b.iter().zip(&v) {
                    dot.0 += ar * vr + ai * vi;
                    dot.1 += ar * vi - ai * vr;
                }
                for (vi, &(br, bi)) in v.iter_mut().zip(b) {
                    vi.0 -= dot.0 * br - dot.1 * bi;
                    vi.1 -= dot.0 * bi + dot.1 * br;
                }
            }
        }
        let norm = v.iter().map(|&(r, i)| r * r + i * i).sum::<f64>().sqrt();
        for z in &mut v {
            z.0 /= norm;
            z.1 /= norm;
        }
        cols.push(v);
    }
    let mut flat = Vec::with_capacity(2 * dim * dim);
    for i in 0..dim {
        for col in &cols {
            flat.push(col[i].0);
            flat.push(col[i].1);
        }
    }
    flat
}

fn main() {
    for (label, n, iters) in [
        ("decompose_2q", 2usize, 5_000usize),
        ("decompose_3q", 3usize, 1_000usize),
        ("decompose_4q", 4usize, 100usize),
    ] {
        let dim = 1usize << n;
        let params = random_unitary_params(dim, 0x1234_5678_9ABC_DEF0 ^ (n as u64));
        let targets: Vec<usize> = (0..n).collect();
        let ops_len = decompose_n(&params, false, &targets).len();
        let time = fastest(5, || {
            for _ in 0..iters {
                black_box(decompose_n(&params, false, &targets));
            }
        });
        println!(
            "{:<14} {:>9.0} mats/s  ({:>6.2} us/mat, {} ops)",
            label,
            iters as f64 / time.as_secs_f64(),
            time.as_secs_f64() * 1e6 / iters as f64,
            ops_len,
        );
    }
}
