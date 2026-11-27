//! Throughput benchmarks for the C embedding API (`include/noev.h`, `spec/interop.md` §3).
//! Run with `cargo bench -p noev-c`.

use std::ffi::CString;
use std::hint::black_box;
use std::ptr;
use std::time::{Duration, Instant};

use noev_c::*;

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

extern "C" fn c_step(
    _ctx: *mut NoevHostContext,
    args: *const NoevValue,
    argc: usize,
    out: *mut NoevValue,
    _user: *mut std::ffi::c_void,
) -> NoevStatus {
    assert_eq!(argc, 1);
    let x = unsafe { (*args).payload.f64 };
    unsafe {
        *out = NoevValue {
            kind: NoevValueKind::Float,
            payload: NoevValuePayload {
                f64: x * 0.999 + 0.001,
            },
        };
    }
    NoevStatus::Ok
}

extern "C" fn c_vqe_opt(
    ctx: *mut NoevHostContext,
    args: *const NoevValue,
    argc: usize,
    out: *mut NoevValue,
    _user: *mut std::ffi::c_void,
) -> NoevStatus {
    assert_eq!(argc, 2);
    let mut theta = unsafe { (*args).payload.f64 };
    let steps = unsafe { (*args.add(1)).payload.i64 };
    let energy_name = c"energy";
    let eps = 0.05;
    let mut last_e = 0.0;
    for _ in 0..steps {
        let mut ep = NoevValue {
            kind: NoevValueKind::Unit,
            payload: NoevValuePayload { u64: 0 },
        };
        let mut em = ep;
        let arg_p = NoevValue {
            kind: NoevValueKind::Float,
            payload: NoevValuePayload { f64: theta + eps },
        };
        let arg_m = NoevValue {
            kind: NoevValueKind::Float,
            payload: NoevValuePayload { f64: theta - eps },
        };
        let s1 = unsafe { noev_host_ctx_call(ctx, energy_name.as_ptr(), &arg_p, 1, &mut ep) };
        if s1 != NoevStatus::Ok {
            return s1;
        }
        let s2 = unsafe { noev_host_ctx_call(ctx, energy_name.as_ptr(), &arg_m, 1, &mut em) };
        if s2 != NoevStatus::Ok {
            return s2;
        }
        let vp = unsafe { ep.payload.f64 };
        let vm = unsafe { em.payload.f64 };
        theta -= 0.4 * (vp - vm) / (2.0 * eps);
        last_e = vp;
    }
    unsafe {
        *out = NoevValue {
            kind: NoevValueKind::Float,
            payload: NoevValuePayload { f64: last_e },
        };
    }
    NoevStatus::Ok
}

fn main() {
    unsafe {
        // 1. C -> Noev classical call throughput.
        let src = CString::new("fn add_mul(a: Float, b: Float) -> Float { a * 1.5 + b }").unwrap();
        let mut prog = ptr::null_mut();
        assert_eq!(
            noev_program_compile_source(src.as_ptr(), &mut prog),
            NoevStatus::Ok
        );
        let mut rt = ptr::null_mut();
        assert_eq!(noev_runtime_new(prog, &mut rt), NoevStatus::Ok);
        let n_calls = 50_000usize;
        let fn_name = c"add_mul";
        let time = fastest(5, || {
            let mut acc = 0.0f64;
            for i in 0..n_calls {
                let args = [
                    NoevValue {
                        kind: NoevValueKind::Float,
                        payload: NoevValuePayload { f64: i as f64 },
                    },
                    NoevValue {
                        kind: NoevValueKind::Float,
                        payload: NoevValuePayload { f64: acc },
                    },
                ];
                let mut out = NoevValue {
                    kind: NoevValueKind::Unit,
                    payload: NoevValuePayload { u64: 0 },
                };
                assert_eq!(
                    noev_runtime_call(rt, fn_name.as_ptr(), args.as_ptr(), 2, &mut out),
                    NoevStatus::Ok
                );
                acc = out.payload.f64 * 0.0001;
            }
            black_box(acc);
        });
        println!(
            "{:<18} {:>10.0} calls/s  ({:.2} ms for {} calls)",
            "c_embed_call",
            n_calls as f64 / time.as_secs_f64(),
            time.as_secs_f64() * 1e3,
            n_calls,
        );
        noev_runtime_free(rt);
        noev_program_free(prog);

        // 2. Noev -> C host callback throughput.
        let src = CString::new(
            "extern fn step(x: Float) -> Float\n\
             fn run_loop(n: Int) -> Float { let mut x = 0.1\n for i in 0..n { x = step(x) }\n x }",
        )
        .unwrap();
        let mut prog = ptr::null_mut();
        assert_eq!(
            noev_program_compile_source(src.as_ptr(), &mut prog),
            NoevStatus::Ok
        );
        let mut rt = ptr::null_mut();
        assert_eq!(noev_runtime_new(prog, &mut rt), NoevStatus::Ok);
        assert_eq!(
            noev_runtime_register_host_fn(rt, c"step".as_ptr(), c_step, ptr::null_mut()),
            NoevStatus::Ok
        );
        let n_host = 100_000i64;
        let time = fastest(5, || {
            let arg = NoevValue {
                kind: NoevValueKind::Int,
                payload: NoevValuePayload { i64: n_host },
            };
            let mut out = NoevValue {
                kind: NoevValueKind::Unit,
                payload: NoevValuePayload { u64: 0 },
            };
            assert_eq!(
                noev_runtime_call(rt, c"run_loop".as_ptr(), &arg, 1, &mut out),
                NoevStatus::Ok
            );
            black_box(out.payload.f64);
        });
        println!(
            "{:<18} {:>10.0} calls/s  ({:.2} ms for {} calls)",
            "c_host_fn_call",
            n_host as f64 / time.as_secs_f64(),
            time.as_secs_f64() * 1e3,
            n_host,
        );
        noev_runtime_free(rt);
        noev_program_free(prog);

        // 3. Re-entrant C VQE optimiser calling Noev quantum expectation values.
        let src = CString::new(
            "extern fn optimize(start: Float, steps: Int) -> Float\n\
             fn ansatz(theta: Float) -> Qubit { let q = |0>\n q.ry(theta)\n mv q }\n\
             fn energy(theta: Float) -> Float { <ansatz(theta)| Z(0) |ansatz(theta)> }\n\
             fn vqe(steps: Int) -> Float { optimize(0.2, steps) }",
        )
        .unwrap();
        let mut prog = ptr::null_mut();
        assert_eq!(
            noev_program_compile_source(src.as_ptr(), &mut prog),
            NoevStatus::Ok
        );
        let mut rt = ptr::null_mut();
        assert_eq!(noev_runtime_new(prog, &mut rt), NoevStatus::Ok);
        assert_eq!(
            noev_runtime_register_host_fn(rt, c"optimize".as_ptr(), c_vqe_opt, ptr::null_mut()),
            NoevStatus::Ok
        );
        let steps = 500i64;
        let evals = (steps * 2) as usize;
        let time = fastest(5, || {
            let arg = NoevValue {
                kind: NoevValueKind::Int,
                payload: NoevValuePayload { i64: steps },
            };
            let mut out = NoevValue {
                kind: NoevValueKind::Unit,
                payload: NoevValuePayload { u64: 0 },
            };
            assert_eq!(
                noev_runtime_call(rt, c"vqe".as_ptr(), &arg, 1, &mut out),
                NoevStatus::Ok
            );
            black_box(out.payload.f64);
        });
        println!(
            "{:<18} {:>10.0} evals/s  ({:.2} ms for {} quantum evals)",
            "c_reentrant_vqe",
            evals as f64 / time.as_secs_f64(),
            time.as_secs_f64() * 1e3,
            evals,
        );
        noev_runtime_free(rt);
        noev_program_free(prog);
    }
}
