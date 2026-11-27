"""Tests for the Noev Python bindings (`spec/interop.md` §1, §2)."""

from dataclasses import dataclass
import math
from pathlib import Path
import pytest

import noev


def test_call_quantum_functions():
    program = noev.Program.compile_source(
        """
fn ansatz(theta: Float) -> Qubit {
    let q = |0>
    q.ry(theta)
    mv q
}

fn energy(theta: Float) -> Float {
    <ansatz(theta)| Z(0) |ansatz(theta)>
}

fn bell(shots: Int) -> Distribution<(Bit, Bit)> {
    sample shots {
        let a = |+>
        let b = |0>
        ctrl a { b.x() }
        (mv a.measure(), mv b.measure())
    }
}
"""
    )
    assert "energy" in program.functions
    assert program.has_function("bell")
    assert program.extern_fns == []

    rt = noev.Runtime(program, seed=42)
    assert math.isclose(rt.call("energy", 0.0), 1.0, abs_tol=1e-12)
    assert math.isclose(rt.call("energy", math.pi), -1.0, abs_tol=1e-12)
    assert math.isclose(rt.call("energy", math.pi / 2.0), 0.0, abs_tol=1e-12)

    dist = rt.call("bell", 100)
    assert isinstance(dist, noev.Distribution)
    assert dist.shots == 100
    assert dist.count((0, 0)) + dist.count((1, 1)) == 100
    assert dist.count((0, 1)) == 0
    assert math.isclose(dist.probability((0, 0)) + dist.probability((1, 1)), 1.0)


def test_all_boundary_types_round_trip():
    program = noev.Program.compile_source(
        """
struct Point {
    label: String,
    x: Float,
    y: Float,
}

enum Status {
    Idle,
    Step(Int, Float),
}

extern fn echo_bigint(n: Int) -> Int
extern fn echo_sized(a: i8, b: u16, c: i32, d: u64, e: f32) -> (i8, u64)
extern fn echo_misc(b: Bool, bit: Bit, z: Complex, z32: c32, s: String, arr: [Float; 3]) -> (Bit, Complex, String, [Float; 3])
extern fn echo_composite(p: Point, st: Status, obs: Observable) -> (Point, Status, Observable)

fn test_bigint(n: Int) -> Int { echo_bigint(n) }
fn test_sized(a: i8, b: u16, c: i32, d: u64, e: f32) -> (i8, u64) { echo_sized(a, b, c, d, e) }
fn test_misc(b: Bool, bit: Bit, z: Complex, z32: c32, s: String, arr: [Float; 3]) -> (Bit, Complex, String, [Float; 3]) {
    echo_misc(b, bit, z, z32, s, arr)
}
fn test_composite(p: Point, st: Status, obs: Observable) -> (Point, Status, Observable) {
    echo_composite(p, st, obs)
}
fn test_unit() {}
"""
    )

    rt = (
        noev.Runtime(program)
        .host_fn("echo_bigint", lambda n: n + 1)
        .host_fn("echo_sized", lambda a, b, c, d, e: (a - 1, d + b + c + int(e)))
        .host_fn(
            "echo_misc",
            lambda b, bit, z, z32, s, arr: (
                1 - bit,
                z * 2 + z32,
                f"{s}!" if b else s,
                list(reversed(arr)),
            ),
        )
        .host_fn("echo_composite", lambda p, st, obs: (p, st, obs))
    )

    # Arbitrary-precision Int beyond 64 bits.
    huge = (1 << 120) + 123456789
    assert rt.call("test_bigint", huge) == huge + 1

    assert rt.call("test_sized", -10, 200, 30, 1000, 2.0) == (-11, 1232)

    bit, z, s, arr = rt.call(
        "test_misc", True, 1, 1.5 - 2.0j, 0.5 + 1.0j, "noev", [1.0, 2.0, 3.0]
    )
    assert bit == 0 and type(bit) is int
    assert z == complex(3.5, -3.0)
    assert s == "noev!"
    assert arr == [3.0, 2.0, 1.0]

    # Classical struct via noev.Struct, dict, and Python dataclass; enum; Observable.
    obs = 0.5 * noev.Observable.z(0) - 0.25 * (noev.Observable.x(0) * noev.Observable.x(1))
    st = noev.Enum("Status", "Step", 7, 0.125)
    pt_in = noev.Struct("Point", label="origin", x=1.0, y=2.0)
    pt_out, st_out, obs_out = rt.call("test_composite", pt_in, st, obs)
    assert pt_out == pt_in
    assert pt_out.label == "origin" and pt_out.x == 1.0 and pt_out["y"] == 2.0
    assert pt_out.to_dict() == {"label": "origin", "x": 1.0, "y": 2.0}
    assert st_out == st and st_out.variant == "Step" and st_out.fields == (7, 0.125)
    assert obs_out == obs

    @dataclass
    class PyPoint:
        label: str
        x: float
        y: float

    pt_from_dc, idle_out, _ = rt.call(
        "test_composite",
        PyPoint("dc", 3.0, 4.0),
        noev.Enum("Status", "Idle"),
        noev.Observable.identity(1.0),
    )
    assert pt_from_dc.label == "dc" and pt_from_dc.x == 3.0
    assert idle_out.variant == "Idle" and idle_out.value is None

    pt_from_dict, _, _ = rt.call(
        "test_composite",
        {"label": "d", "x": 5.0, "y": 6.0},
        noev.Enum("Status", "Idle"),
        noev.Observable.z(0),
    )
    assert pt_from_dict.y == 6.0

    assert rt.call("test_unit") is None


def test_pass_observable_into_expectation_value():
    program = noev.Program.compile_source(
        """
fn bell_state() -> [Qubit; 2] {
    let r = |00>
    r[0].h()
    ctrl r[0] { r[1].x() }
    mv r
}

fn measure_h(h: Observable) -> Float {
    <bell_state()| h |bell_state()>
}
"""
    )
    rt = noev.Runtime(program)
    zz = noev.Observable.z(0) * noev.Observable.z(1)
    xx = noev.Observable.x(0) * noev.Observable.x(1)
    h = 0.5 * zz + 0.5 * xx + noev.Observable.identity(0.25)
    assert math.isclose(rt.call("measure_h", h), 1.25, abs_tol=1e-12)


def test_vqe_host_optimiser_from_noev():
    """Noev drives a stateful Python optimiser through `extern fn` (`spec/interop.md` §2)."""
    program = noev.Program.compile_source(
        """
extern fn minimize_step(energy: Float, theta: Float) -> Float

fn ansatz(theta: Float) -> Qubit {
    let q = |0>
    q.ry(theta)
    mv q
}

fn vqe() -> Float {
    let h = Z(0)
    let mut theta = 0.2
    for i in 0..40 {
        theta = minimize_step(<ansatz(theta)| h |ansatz(theta)>, theta)
    }
    <ansatz(theta)| h |ansatz(theta)>
}
"""
    )

    class SecantOptimiser:
        def __init__(self, lr: float = 0.4) -> None:
            self.lr = lr
            self.prev: tuple[float, float] | None = None
            self.steps = 0

        def step(self, energy: float, theta: float) -> float:
            self.steps += 1
            if self.prev is None:
                nxt = theta + 0.1
            else:
                prev_e, prev_t = self.prev
                dt = theta - prev_t
                nxt = theta if abs(dt) < 1e-12 else theta - self.lr * (energy - prev_e) / dt
            self.prev = (energy, theta)
            return nxt

    opt = SecantOptimiser()
    rt = noev.Runtime(program).host_fn("minimize_step", opt.step)
    energy = rt.call("vqe")
    assert opt.steps == 40
    assert math.isclose(energy, -1.0, abs_tol=1e-6)


def test_reentrant_callback_and_error_handling():
    program = noev.Program.compile_source(
        """
extern fn optimize(start: Float) -> Float
extern fn try_boom() -> Int

fn ansatz(theta: Float) -> Qubit {
    let q = |0>
    q.ry(theta)
    mv q
}

fn energy(theta: Float) -> Float {
    <ansatz(theta)| Z(0) |ansatz(theta)>
}

fn boom() -> Int {
    assert false
    1
}

fn run_vqe() -> Float {
    let theta = optimize(0.3)
    energy(theta)
}

fn run_try_boom() -> Int {
    try_boom()
}
"""
    )

    stashed_ctx: list[noev.HostContext] = []

    def optimize(ctx: noev.HostContext, start: float) -> float:
        stashed_ctx.append(ctx)
        theta = start
        eps = 0.05
        for _ in range(30):
            ep = ctx.call("energy", theta + eps)
            em = ctx.call("energy", theta - eps)
            theta -= 0.5 * (ep - em) / (2.0 * eps)
        return theta

    def try_boom(ctx: noev.HostContext) -> int:
        try:
            return ctx.call("boom")
        except noev.RuntimeError:
            return 42

    rt = (
        noev.Runtime(program)
        .host_fn_with_context("optimize", optimize)
        .host_fn_with_context("try_boom", try_boom)
    )
    assert math.isclose(rt.call("run_vqe"), -1.0, abs_tol=1e-6)
    assert rt.call("run_try_boom") == 42

    # Stashed HostContext cannot be used after the host callback has returned.
    with pytest.raises(noev.NoevError, match="only valid"):
        stashed_ctx[0].call("energy", 0.0)


def test_scipy_vqe_two_qubit_hamiltonian():
    """SciPy's BFGS optimiser driving a 2-qubit Noev expectation value."""
    scipy_opt = pytest.importorskip("scipy.optimize")

    program = noev.Program.compile_source(
        """
extern fn scipy_minimize(start: [Float; 4]) -> [Float; 4]

fn ansatz(params: [Float; 4]) -> [Qubit; 2] {
    let r = |00>
    r[0].ry(params[0])
    r[1].ry(params[1])
    ctrl r[0] { r[1].x() }
    r[0].ry(params[2])
    r[1].ry(params[3])
    mv r
}

fn energy(params: [Float; 4]) -> Float {
    // H = Z0 Z1 + 0.5 X0 + 0.5 X1; exact ground-state energy is -sqrt(2) ≈ -1.41421356.
    let h = Z(0) * Z(1) + 0.5 * (X(0) + X(1))
    <ansatz(params)| h |ansatz(params)>
}

fn vqe() -> Float {
    let best = scipy_minimize([0.2, 0.3, 0.4, 0.5])
    energy(best)
}
"""
    )

    def run_scipy(ctx: noev.HostContext, start: list[float]) -> list[float]:
        res = scipy_opt.minimize(
            lambda x: ctx.call("energy", [float(v) for v in x]),
            start,
            method="BFGS",
        )
        return [float(v) for v in res.x]

    rt = noev.Runtime(program).host_fn_with_context("scipy_minimize", run_scipy)
    ground = rt.call("vqe")
    # Also verify direct Python->Noev optimisation with scipy.optimize.minimize.
    direct = scipy_opt.minimize(
        lambda x: rt.call("energy", [float(v) for v in x]),
        [0.2, 0.3, 0.4, 0.5],
        method="BFGS",
    )
    expected = -math.sqrt(2.0)
    assert math.isclose(ground, expected, abs_tol=1e-5)
    assert math.isclose(direct.fun, expected, abs_tol=1e-5)


def test_python_exception_propagates_intact():
    program = noev.Program.compile_source(
        """
extern fn fail_host(x: Int) -> Int
fn entry() -> Int { fail_host(1) }
"""
    )

    class CustomBoom(Exception):
        pass

    def bad(_x: int) -> int:
        raise CustomBoom("from python host")

    rt = noev.Runtime(program).host_fn("fail_host", bad)
    with pytest.raises(CustomBoom, match="from python host"):
        rt.call("entry")


def test_validation_and_type_errors():
    with pytest.raises(noev.CompileError):
        noev.Program.compile_source("extern fn bad(q: Qubit)")

    program = noev.Program.compile_source(
        """
extern fn add(a: Float, b: Int) -> Float
fn run() -> Float { add(1.0, 2) }
fn takes_qubit(q: Qubit) { mv q.discard() }
fn takes_bit(b: Bit) -> Bit { b }
fn takes_u8(x: u8) -> u8 { x }
"""
    )

    # Missing host fn.
    rt = noev.Runtime(program)
    with pytest.raises(noev.ValidationError, match="missing host"):
        rt.validate()

    # Wrong arity caught at validate().
    rt_bad_arity = noev.Runtime(program).host_fn("add", lambda a: a)
    with pytest.raises(noev.ValidationError, match="incompatible signature"):
        rt_bad_arity.validate()

    # Unknown host fn name.
    rt_unknown = (
        noev.Runtime(program)
        .host_fn("add", lambda a, b: a + b)
        .host_fn("nope", lambda: 0)
    )
    with pytest.raises(noev.ValidationError, match="not declared"):
        rt_unknown.validate()

    # Boundary type check, unknown function, bad arg types, overflow, bool-as-int rejection.
    rt_ok = noev.Runtime(program).host_fn("add", lambda a, b: a + b)
    assert rt_ok.call("run") == 3.0

    with pytest.raises(noev.ValidationError, match="cannot cross"):
        rt_ok.call("takes_qubit", None)
    with pytest.raises(noev.ValidationError, match="no function"):
        rt_ok.call("does_not_exist")
    with pytest.raises(ValueError, match="0 or 1"):
        rt_ok.call("takes_bit", 2)
    with pytest.raises(TypeError):
        rt_ok.call("takes_bit", True)
    with pytest.raises(OverflowError):
        rt_ok.call("takes_u8", 300)
    with pytest.raises(TypeError):
        rt_ok.call("takes_u8", True)


def test_compile_file_output_capture_and_export_backend(tmp_path: Path):
    src = tmp_path / "prog.nv"
    src.write_text(
        """
extern fn double_it(n: Int) -> Int

fn greet(n: Int) -> Int {
    print("hello", n)
    double_it(n)
}
"""
    )
    program = noev.Program.compile_file(str(src))
    rt = (
        noev.Runtime(program, backend="export")
        .capture_output(True)
        .host_fn("double_it", lambda n: n * 2)
    )
    assert rt.call("greet", 21) == 42
    assert rt.last_stdout == "hello 21\n"
    assert rt.last_stderr == ""
