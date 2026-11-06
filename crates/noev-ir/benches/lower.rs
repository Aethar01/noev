//! Throughput benchmark for lowering to the IR. Run with `cargo bench -p noev-ir`.
//!
//! Each accepted UI program is checked once, then lowered many times. Dependency-free, like
//! the other benchmarks: the fastest of several runs is reported.

use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/ui/pass");
    let mut sources: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
        .collect();
    sources.sort();
    let checked: Vec<_> = sources
        .iter()
        .map(|s| {
            let parsed = noev_syntax::parse(s);
            let (typed, _) = noev_sema::check(&parsed.ast);
            (s, parsed, typed)
        })
        .collect();
    let repeats = 500;
    let bytes: usize = sources.iter().map(String::len).sum::<usize>() * repeats;
    let lines: usize = sources.iter().map(|s| s.lines().count()).sum::<usize>() * repeats;

    let time = (0..10)
        .map(|_| {
            let start = Instant::now();
            for _ in 0..repeats {
                for (source, parsed, typed) in &checked {
                    black_box(noev_ir::lower(black_box(&parsed.ast), typed, source));
                }
            }
            start.elapsed()
        })
        .min()
        .unwrap();
    let secs = time.as_secs_f64();
    let mib = bytes as f64 / (1024.0 * 1024.0);
    println!(
        "lower     {:>8.1} MiB/s {:>10.0} lines/s  ({:.2} ms for {mib:.1} MiB)",
        mib / secs,
        lines as f64 / secs,
        secs * 1e3,
    );
}
