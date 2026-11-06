# Core

The intrinsics: operations the compiler and runtime implement directly. Everything else
that comes with Noev is in the standard library ([std.md](std.md)), defined in terms of
these. Both are in scope in every program without imports (see [std.md](std.md) §1).

**What belongs in core.** An operation is an intrinsic if either:

1. Noev code cannot express it: creating and measuring qubits, `reset`, `release`, maths
   such as `sin`, conversions, the Pauli operators; or
2. the compiler or hardware must recognise it by name, even though it could be derived.
   Mathematically `{h, t, ctrl x}` is enough for any circuit, but defining `x` as
   `u(PI, 0, PI)` would make the optimiser and the OpenQASM backend reverse-engineer it,
   lose the simulator's fast special cases, and lose hardware native gates. So the gates of
   OpenQASM's standard gate library are intrinsics.

Anything that meets neither condition belongs in the standard library.

## 1. Gates

Gates are built-in methods of `Qubit`. They take `mut self` and return `mut Self` so they
can be chained (`q.h().s()`), and are reversible, so they can be used in reversible
functions, `ctrl` blocks and under `adj`.

| Method                                             | Effect                    |
| -------------------------------------------------- | ------------------------- |
| `fn x(mut self) -> mut Self`                       | Pauli X                   |
| `fn y(mut self) -> mut Self`                       | Pauli Y                   |
| `fn z(mut self) -> mut Self`                       | Pauli Z                   |
| `fn h(mut self) -> mut Self`                       | Hadamard                  |
| `fn s(mut self) -> mut Self`                       | S = √Z                    |
| `fn t(mut self) -> mut Self`                       | T = ⁴√Z                   |
| `fn rx(mut self, theta: Float) -> mut Self`        | exp(−iθX/2)               |
| `fn ry(mut self, theta: Float) -> mut Self`        | exp(−iθY/2)               |
| `fn rz(mut self, theta: Float) -> mut Self`        | exp(−iθZ/2)               |
| `fn p(mut self, lambda: Float) -> mut Self`        | diag(1, e^iλ)             |
| `fn u(mut self, theta: Float, phi: Float, lambda: Float) -> mut Self` | OpenQASM `U` |
| `fn swap(mut self, other: mut Qubit) -> mut Self`  | Swap two qubits           |

Every gate is also a free function with the qubits first: `x(q)`, `ry(q, theta)`,
`swap(a, b)`. The free functions return `()`. Qubit arguments are lent implicitly, so no
`mut` is needed at the call.

The set is deliberately small:

- **No controlled gates.** `cx`, `cz`, `ccx`, `cry` and the rest are written with `ctrl`:
  `ctrl a { b.x() }`. The compiler lowers common cases to native gates.
- **No inverse gates.** `sdg` and `tdg` are `adj q.s()` and `adj q.t()`.

## 2. Measurement and reset

Not reversible, so not allowed in a reversible context.

| Method                                  | Effect                                          |
| --------------------------------------- | ----------------------------------------------- |
| `fn measure(mv self) -> Bit`            | Measure in the Z basis; consumes the qubit      |
| `fn reset(mut self) -> mut Self`        | Return to \|0⟩; the qubit stays usable          |
| `unsafe fn release(mv self)`            | Release without a reset, trusting the qubit is in \|0⟩ (see [ownership.md](ownership.md) §11) |

`discard`, which resets and releases, is in the standard library.

`[Qubit; N]` also has `fn measure(mv self) -> [Bit; N]`, which measures every qubit and
consumes the array. (This is the `Measure` trait once traits exist; see
[types.md](types.md) §6.) It is an intrinsic because writing it in Noev needs generics over
the length `N`, and moving elements out with a loop index is not allowed (N012).

Also as free functions: `measure(mv q)`, `reset(q)`.

## 3. Measurement tests

Measurement in the X basis, and tests against specific states, are written with bras (see
[grammar.md](grammar.md) §2, Bras and expectation values): `<+| mv q` is `true` if `q` is
measured as \|+⟩.

## 4. Observables

| Function / operator                      | Meaning                                      |
| ---------------------------------------- | -------------------------------------------- |
| `X(i: Int) -> Observable`                | Pauli X on qubit `i` of the preparation      |
| `Y(i: Int) -> Observable`                | Pauli Y on qubit `i`                         |
| `Z(i: Int) -> Observable`                | Pauli Z on qubit `i`                         |
| `I() -> Observable`                      | The identity (for constant offsets)          |
| `a + b`, `a - b`, `-a`                   | Sums                                         |
| `c * a`, `a * c` with `c: Float`         | Scaling                                      |
| `a * b`                                  | Product; the factors must act on different qubits (a qubit is acted on if any term has a non-identity Pauli on it), so the result is Hermitian |

```noev
let ising = -1.0 * Z(0) * Z(1) + 0.5 * (X(0) + X(1))
let energy = <ansatz(theta)| ising |ansatz(theta)>
```

Projectors `P` and `PX` are built from these in the standard library.

## 5. Math

Methods on `Float`: `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2(self, x)`, `sqrt`,
`exp`, `ln`, `abs`, `min(self, other)`, `max(self, other)`. `Int` has `abs`, `min` and
`max`. Called as `theta.sin()` or `Float::sin(theta)`.

Conversions from `Float` to `Int`: `trunc`, `round`, `floor`, `ceil` (see
[types.md](types.md) §4).

Constants: `Float::PI`, `Float::TAU`, `Float::E`.

All of these are pure, so they may be called in reversible contexts.

## 6. Running a program

A program runs by calling `main` once per **shot**. What `main` returns is the shot's result,
and the results of all shots are counted.

- **Classical results** (bits, integers, tuples, structs…) are reported as they are. Bits,
  and tuples and arrays of bits, are written as bit strings in index order, the same order
  as kets: `|01>` measured gives `01`.
- **Qubits** may be returned too: `main` hands them to the runtime, which measures them in
  the Z basis after the shot (so returning them consumes them). This is the way to ask for
  **sampling**: when `main` measures nothing itself, every shot produces the same state, so
  a simulator computes it once and samples it as many times as needed. When `main` does
  measure (for feedback, say), each shot runs separately.

```noev
fn main() -> [Qubit; 2] {        // sampled: the state is computed once
    let r = |+0>
    ctrl r[0] { r[1].x() }
    mv r
}
```

Returned qubits are measured in the order of the value's qubits: elements and fields left
to right.

## 7. Open questions

- **Measuring without consuming.** `measure` consumes the qubit. Repeated mid-circuit
  measurement (error-correction syndromes, for example) would prefer something like
  `fn measure_reset(mut self) -> Bit` that leaves the qubit in \|0⟩.
- **Estimation settings.** How many runs an estimated expectation value uses (on hardware,
  or when the preparation measures), and how to set it per expectation or per program.
- **Y-basis bras** (`<i|`, `<-i|`), alongside Y-basis kets.
- **Printing and output** for classical results, beyond the CLI's result histogram.
- **Sampling inside a program**: a `sample` of a preparation returning a distribution, for
  hybrid loops that post-process measurement statistics.
