//! Throughput benchmark for type checking. Run with `cargo bench -p noev-sema`.
//!
//! Each UI test program is parsed once, then checked many times; repeating programs within
//! one file would define every function twice. Dependency-free, like the syntax benchmark:
//! the fastest of several runs is reported.

use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

fn programs() -> Vec<String> {
    let mut files = Vec::new();
    let mut stack = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/ui")];
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            stack.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
        } else if path.extension().is_some_and(|e| e == "nv") {
            files.push(std::fs::read_to_string(&path).unwrap());
        }
    }
    files.sort();
    files
}

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
    let sources = programs();
    let parsed: Vec<_> = sources.iter().map(|s| noev_syntax::parse(s)).collect();
    let repeats = 500;
    let bytes: usize = sources.iter().map(String::len).sum::<usize>() * repeats;
    let lines: usize = sources.iter().map(|s| s.lines().count()).sum::<usize>() * repeats;

    let time = fastest(10, || {
        for _ in 0..repeats {
            for p in &parsed {
                black_box(noev_sema::check(black_box(&p.ast)));
            }
        }
    });
    let secs = time.as_secs_f64();
    let mib = bytes as f64 / (1024.0 * 1024.0);
    println!(
        "check    {:>8.1} MiB/s {:>10.0} lines/s  ({:.2} ms for {mib:.1} MiB)",
        mib / secs,
        lines as f64 / secs,
        secs * 1e3,
    );
}
