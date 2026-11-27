"""Throughput benchmarks for the Noev Python bindings (`spec/interop.md` §2).

Run after building the extension (`maturin develop --release`):
    python python/bench_embed.py
"""

import time
import noev


def fastest(runs: int, fn) -> float:
    best = float("inf")
    for _ in range(runs):
        t0 = time.perf_counter()
        fn()
        dt = time.perf_counter() - t0
        if dt < best:
            best = dt
    return best


def main() -> None:
    # 1. Python -> Noev classical call throughput.
    prog_call = noev.Program.compile_source(
        """
fn add_mul(a: Float, b: Float) -> Float {
    a * 1.5 + b
}
"""
    )
    rt_call = noev.Runtime(prog_call)
    n_calls = 50_000

    def bench_call() -> None:
        acc = 0.0
        call = rt_call.call
        for i in range(n_calls):
            acc = call("add_mul", float(i), acc) * 0.0001

    dt = fastest(5, bench_call)
    print(f"{'py_embed_call':<18} {n_calls / dt:>10.0f} calls/s  ({dt * 1e3:.2f} ms for {n_calls} calls)")

    # 2. Noev -> Python `extern fn` callback throughput.
    prog_host = noev.Program.compile_source(
        """
extern fn step(x: Float) -> Float

fn run_loop(n: Int) -> Float {
    let mut x = 0.1
    for i in 0..n {
        x = step(x)
    }
    x
}
"""
    )
    rt_host = noev.Runtime(prog_host).host_fn("step", lambda x: x * 0.999 + 0.001)
    n_host = 50_000
    dt = fastest(5, lambda: rt_host.call("run_loop", n_host))
    print(f"{'py_host_fn_call':<18} {n_host / dt:>10.0f} calls/s  ({dt * 1e3:.2f} ms for {n_host} calls)")

    # 3. Re-entrant Python VQE calling Noev quantum expectation values.
    prog_vqe = noev.Program.compile_source(
        """
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
"""
    )

    def opt(ctx: noev.HostContext, start: float, steps: int) -> float:
        theta = start
        eps = 0.05
        e = 0.0
        for _ in range(steps):
            ep = ctx.call("energy", theta + eps)
            em = ctx.call("energy", theta - eps)
            theta -= 0.4 * (ep - em) / (2.0 * eps)
            e = ep
        return e

    rt_vqe = noev.Runtime(prog_vqe).host_fn_with_context("optimize", opt)
    steps = 500
    evals = steps * 2
    dt = fastest(5, lambda: rt_vqe.call("vqe", steps))
    print(f"{'py_reentrant_vqe':<18} {evals / dt:>10.0f} evals/s  ({dt * 1e3:.2f} ms for {evals} quantum evals)")


if __name__ == "__main__":
    main()
