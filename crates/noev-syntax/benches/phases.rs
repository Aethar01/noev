//! Throughput benchmarks for the syntax phases, on a large program built by repeating the
//! UI test suite. Run with `cargo bench -p noev-syntax`.
//!
//! Dependency-free on purpose: each phase runs several times and the fastest run is
//! reported, which is stable enough to catch regressions.

use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

/// Concatenates every `.nv` file under `tests/ui` until the result is at least `target` bytes.
fn corpus(target: usize) -> String {
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
    let mut source = String::with_capacity(target + 4096);
    while source.len() < target {
        for file in &files {
            source.push_str(file);
            source.push('\n');
        }
    }
    source
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

fn report(phase: &str, source: &str, time: Duration) {
    let mib = source.len() as f64 / (1024.0 * 1024.0);
    let lines = source.lines().count() as f64;
    let secs = time.as_secs_f64();
    println!(
        "{phase:<8} {:>8.1} MiB/s {:>10.0} lines/s  ({:.2} ms for {mib:.1} MiB)",
        mib / secs,
        lines / secs,
        secs * 1e3,
    );
}

fn main() {
    let source = corpus(16 * 1024 * 1024);
    let time = fastest(10, || {
        black_box(noev_syntax::lex(black_box(&source)));
    });
    report("lex", &source, time);

    let time = fastest(10, || {
        black_box(noev_syntax::parse(black_box(&source)));
    });
    report("parse", &source, time);
}
