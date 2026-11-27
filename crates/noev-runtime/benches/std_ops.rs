//! Throughput benchmark for Step 16 standard-library operations (`Vec<T>`, `String`).
//! Run with `cargo bench -p noev-runtime --bench std_ops`.

use std::hint::black_box;
use std::time::{Duration, Instant};

use noev_runtime::backend::Exporter;
use noev_runtime::cabi::NoevRuntime;
use noev_runtime::int::Int;
use noev_runtime::std_ops::eval;
use noev_runtime::value::Value;
use noev_sema::prelude::StdOp;

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
    let mut rt = NoevRuntime::new_owned(Box::new(Exporter::default()), 1);

    // 1. Vec push + index throughput.
    let n = 50_000usize;
    let time = fastest(5, || {
        let v = eval(
            &mut rt,
            StdOp::VecWithCapacity,
            vec![Value::Int(Int::from(n as i64))],
        )
        .unwrap();
        for i in 0..n {
            eval(
                &mut rt,
                StdOp::VecPush,
                vec![v.clone(), Value::Int(Int::from(i as i64))],
            )
            .unwrap();
        }
        let mut sum = 0i64;
        for i in 0..n {
            let val = eval(
                &mut rt,
                StdOp::VecIndex,
                vec![v.clone(), Value::Int(Int::from(i as i64))],
            )
            .unwrap();
            if let Value::Int(x) = val {
                sum += x.to_i64().unwrap_or(0);
            }
        }
        black_box(sum);
    });
    println!(
        "{:<16} {:>10.0} ops/s   ({:.2} ms for {} push+index ops)",
        "std_vec_ops",
        (2 * n) as f64 / time.as_secs_f64(),
        time.as_secs_f64() * 1e3,
        2 * n,
    );

    // 2. String split + join throughput (typical OpenQASM / JSON backend text processing).
    let rounds = 10_000usize;
    let line = Value::Str("h q[0]; cx q[0], q[1]; rz(0.5) q[1]; measure q[0] -> c[0];".into());
    let sep = Value::Str("; ".into());
    let out_sep = Value::Str("\n".into());
    let time = fastest(5, || {
        for _ in 0..rounds {
            let parts = eval(&mut rt, StdOp::StrSplit, vec![line.clone(), sep.clone()]).unwrap();
            let joined = eval(&mut rt, StdOp::StrJoin, vec![parts, out_sep.clone()]).unwrap();
            black_box(joined);
        }
    });
    println!(
        "{:<16} {:>10.0} pairs/s ({:.2} ms for {} split+join pairs)",
        "std_str_split",
        rounds as f64 / time.as_secs_f64(),
        time.as_secs_f64() * 1e3,
        rounds,
    );
}
