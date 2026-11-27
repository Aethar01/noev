//! Throughput benchmarks for the Rust embedding API (`spec/interop.md` §1, §2).
//! Run with `cargo bench -p noev`.

use std::hint::black_box;
use std::time::{Duration, Instant};

use noev::{HostContext, Program, Runtime};

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
    // 1. Rust -> Noev call overhead (pure classical function).
    let prog_call = Program::compile_source(
        r#"
fn add_mul(a: Float, b: Float) -> Float {
    a * 1.5 + b
}
"#,
    )
    .unwrap();
    let mut rt_call = Runtime::new(&prog_call);
    let n_calls = 50_000usize;
    let time = fastest(5, || {
        let mut acc = 0.0f64;
        for i in 0..n_calls {
            let y: f64 = rt_call.call("add_mul", (i as f64, acc)).unwrap();
            acc = y * 0.0001;
        }
        black_box(acc);
    });
    println!(
        "{:<18} {:>10.0} calls/s  ({:.2} ms for {} calls)",
        "embed_call",
        n_calls as f64 / time.as_secs_f64(),
        time.as_secs_f64() * 1e3,
        n_calls,
    );

    // 2. Noev -> Rust `extern fn` callback overhead inside a Noev loop.
    let prog_host = Program::compile_source(
        r#"
extern fn step(x: Float) -> Float

fn run_loop(n: Int) -> Float {
    let mut x = 0.1
    for i in 0..n {
        x = step(x)
    }
    x
}
"#,
    )
    .unwrap();
    let mut rt_host = Runtime::new(&prog_host).host_fn("step", |x: f64| x * 0.999 + 0.001);
    let n_host = 100_000i64;
    let time = fastest(5, || {
        let y: f64 = rt_host.call("run_loop", (n_host,)).unwrap();
        black_box(y);
    });
    println!(
        "{:<18} {:>10.0} calls/s  ({:.2} ms for {} calls)",
        "host_fn_call",
        n_host as f64 / time.as_secs_f64(),
        time.as_secs_f64() * 1e3,
        n_host,
    );

    // 3. Re-entrant Rust optimiser calling Noev quantum expectation values.
    let prog_vqe = Program::compile_source(
        r#"
extern fn optimize(start: Float, steps: Int) -> Float

fn ansatz(theta: Float) -> Qubit {
    let q = |0>
    q.ry(theta)
    mv q
}

fn energy(theta: Float) -> Float {
    <ansatz(theta)| Z(0) |ansatz(theta)>
}

fn vqe(steps: Int) -> Float {
    optimize(0.2, steps)
}
"#,
    )
    .unwrap();
    let mut rt_vqe = Runtime::new(&prog_vqe).host_fn_with_context(
        "optimize",
        |ctx: &mut HostContext<'_, '_>, start: f64, steps: i64| -> Result<f64, noev::Error> {
            let mut theta = start;
            let eps = 0.05;
            let mut e = 0.0;
            for _ in 0..steps {
                let e_plus: f64 = ctx.call("energy", (theta + eps,))?;
                let e_minus: f64 = ctx.call("energy", (theta - eps,))?;
                let grad = (e_plus - e_minus) / (2.0 * eps);
                theta -= 0.4 * grad;
                e = e_plus;
            }
            Ok(e)
        },
    );
    let steps = 500i64; // 1000 quantum expectation evaluations per run
    let evals = (steps * 2) as usize;
    let time = fastest(5, || {
        let e: f64 = rt_vqe.call("vqe", (steps,)).unwrap();
        black_box(e);
    });
    println!(
        "{:<18} {:>10.0} evals/s  ({:.2} ms for {} quantum evals)",
        "reentrant_vqe",
        evals as f64 / time.as_secs_f64(),
        time.as_secs_f64() * 1e3,
        evals,
    );
}
