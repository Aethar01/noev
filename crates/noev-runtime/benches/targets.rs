//! Benchmark for Step 17 target TOML parsing/serialisation and compile-time capability checks.
//! Run with `cargo bench -p noev-runtime --bench targets`.

use std::hint::black_box;
use std::time::{Duration, Instant};

use noev_sema::{TargetSpec, check_target};

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
    // 1. TargetSpec TOML round-trip throughput.
    let spec = TargetSpec::from_toml(
        r#"
name = "qpu_bench"
device = "heron_r2"
level = "circuit"
qubits = 156
gates = ["rx", "rz", "sx", "x", "cz"]
coupling = [[0, 1], [1, 2], [2, 3], [3, 4], [4, 5], [5, 6], [6, 7]]

[capabilities]
read_state = false
check_release = false
exact_expectations = false
sample_state = false
foreign_while_live = false
"#,
    )
    .unwrap();
    let toml_text = spec.to_toml();
    let rounds = 20_000usize;
    let time = fastest(5, || {
        for _ in 0..rounds {
            let parsed = TargetSpec::from_toml(black_box(&toml_text)).unwrap();
            black_box(parsed.to_toml());
        }
    });
    println!(
        "{:<18} {:>10.0} rt/s    ({:.2} ms for {} TOML round-trips)",
        "target_toml",
        rounds as f64 / time.as_secs_f64(),
        time.as_secs_f64() * 1e3,
        rounds,
    );

    // 2. Compile-time `check_target` throughput on a realistic Bell + GHZ program.
    let src = r#"
fn bell() -> (Bit, Bit) {
    let mut a = |0>
    let mut b = |0>
    h(mut a)
    ctrl a { x(mut b) }
    (mv a.measure(), mv b.measure())
}

fn ghz4() -> [Bit; 4] {
    let mut q = |0000>
    h(mut q[0])
    ctrl q[0] { x(mut q[1]) }
    ctrl q[1] { x(mut q[2]) }
    ctrl q[2] { x(mut q[3]) }
    mv q.measure()
}

fn main() {
    let _ = sample 100 { bell() }
    let _ = sample 100 { ghz4() }
}
"#;
    let parsed = noev_syntax::parse(src);
    let (typed, _) = noev_sema::check(&parsed.ast);
    let (circuits, _) = noev_sema::check_circuits(&parsed.ast, &typed);

    let checks = 50_000usize;
    let time = fastest(5, || {
        for _ in 0..checks {
            let diags = check_target(&parsed.ast, &typed, &circuits, &spec);
            black_box(diags.len());
        }
    });
    println!(
        "{:<18} {:>10.0} chk/s   ({:.2} ms for {} target capability checks)",
        "check_target",
        checks as f64 / time.as_secs_f64(),
        time.as_secs_f64() * 1e3,
        checks,
    );
}
