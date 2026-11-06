# Prelude

Types, methods and functions available in every program without imports.

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
| `fn discard(mv self)`                   | Reset and release; consumes the qubit           |
| `unsafe fn release(mv self)`            | Release without a reset, trusting the qubit is in \|0⟩ (see [ownership.md](ownership.md) §11) |

`[Qubit; N]` also has `fn measure(mv self) -> [Bit; N]`, which measures every qubit and
consumes the array. (This is the `Measure` trait once traits exist; see
[types.md](types.md) §6.)

Also as free functions: `measure(mv q)`, `reset(q)`, `discard(mv q)`.

## 3. Math

Methods on `Float`: `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2(self, x)`, `sqrt`,
`exp`, `ln`, `abs`, `min(self, other)`, `max(self, other)`. `Int` has `abs`, `min` and
`max`. Called as `theta.sin()` or `Float::sin(theta)`.

Conversions from `Float` to `Int`: `trunc`, `round`, `floor`, `ceil` (see
[types.md](types.md) §4).

Constants: `Float::PI`, `Float::TAU`, `Float::E`.

All of these are pure, so they may be called in reversible contexts.

## 4. Open questions

- **Measuring without consuming.** `measure` consumes the qubit. Repeated mid-circuit
  measurement (error-correction syndromes, for example) would prefer something like
  `fn measure_reset(mut self) -> Bit` that leaves the qubit in \|0⟩.
- **Other measurement bases**, such as `measure_x`.
- **Printing and output** for classical results, beyond the CLI's result histogram.
