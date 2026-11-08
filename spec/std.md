# Standard library

Everything that comes with Noev but is not an intrinsic ([core.md](core.md)). Each item is
specified by a **reference definition** written in Noev, using only core and other
standard-library items.

**Implementation in v0.1.** The compiler implements the standard library natively, in Rust,
so it costs nothing to compile or check. Each native item must behave exactly as its
reference definition: same signature, same ownership rules, same reversibility, same
results. An item can therefore move to Noev source later without any program noticing.

## 1. What is in scope

In v0.1, every item in [core.md](core.md) and in this document is in scope in every program,
without imports. A program's own function with the same name takes precedence.

Once Noev has modules ([modules.md](modules.md)), the standard library becomes modules under
`std::`, and the **prelude** becomes the set of names imported into every module
automatically.

## 2. Qubits

### `discard`

Resets a qubit and releases it. Consumes the qubit; not reversible.

```noev
fn discard(mv q: Qubit) {
    q.reset()
    unsafe { mv q.release() }
}
```

Also a method of `Qubit`: `mv q.discard()`. (Written as a method once Noev can add methods to
built-in types.)

This is the safe way to give back a qubit in an unknown state. `release` (core, `unsafe`)
skips the reset and trusts the qubit is already in \|0⟩.

### `Basis` and `measure_in`

Measures a qubit in the X, Y or Z basis. `measure` is the Z basis; the others are a rotation
into it first.

```noev
/// A measurement basis: the eigenbasis of a Pauli operator.
enum Basis { X, Y, Z }

/// Measures in `basis`: 0 for the +1 eigenstate (|0⟩, |+⟩, |+i⟩), 1 for the −1 eigenstate
/// (|1⟩, |−⟩, |−i⟩), so the outcome is always (−1)^bit.
fn measure_in(mv q: Qubit, basis: Basis) -> Bit {
    match basis {
        Basis::X => { q.h() }
        Basis::Y => { adj q.s(); q.h() }
        Basis::Z => {}
    }
    mv q.measure()
}
```

Also a method, `mv q.measure_in(Basis::X)`, and on `[Qubit; N]`, measuring every qubit in
the same basis and giving `[Bit; N]`. Consumes the qubits; not reversible. The basis is an
ordinary value, so it can be computed; choosing it from a measurement result counts like a
gate angle chosen from one ([execution.md](execution.md) §4).

A measurement test is a shortcut for the same thing: `<+| mv q` is
`mv q.measure_in(Basis::X) == 0`.

### `State`

The six basis states a ket or bra label names, as a value, so a label can be computed
(`|{s}>`, [types.md](types.md) §3).

```noev
/// One qubit's basis state: the labels `0 1 + - (+i) (-i)`.
enum State { Zero, One, Plus, Minus, PlusI, MinusI }
```

As with `Basis`, a program's own type called `State` takes the name, and kets still accept
the standard one.

### `amplitudes` (simulators only)

For debugging: the amplitudes of a register's state, as a simulator holds them.

```noev
let r = |+0>
ctrl r[0] { r[1].x() }
print(amplitudes(r))     // [0.7071…+0.0i, 0.0+0.0i, 0.0+0.0i, 0.7071…+0.0i]
```

`amplitudes(r)` lends `r`, a `Qubit` or `[Qubit; N]` with N at most 20, and gives a
`[Complex; 2^N]` over its basis states in the order kets read them: \|00⟩, \|01⟩, \|10⟩,
\|11⟩, with `r[0]` the left label. It changes nothing. A register entangled with other qubits
has no state of its own, so asking for one is a runtime error, as it is on a backend that
cannot see the state. The overall phase is chosen so that the first amplitude that is not zero
is real and positive; only the relative phases between amplitudes mean anything physically.

It is not reversible, since under `adj` a function's gates are collected rather than run, and
a `sample` that reads amplitudes runs shot by shot.

## 3. Observables

### `P` and `PX`

Projectors onto one qubit's basis states, as sums of Paulis.

```noev
/// Projector onto |b⟩ in the Z basis for qubit `i`.
fn P(i: Int, b: Bit) -> Observable {
    if b == 0 { 0.5 * (I() + Z(i)) } else { 0.5 * (I() - Z(i)) }
}

/// Projector onto |+⟩ (b = 0) or |−⟩ (b = 1) for qubit `i`.
fn PX(i: Int, b: Bit) -> Observable {
    if b == 0 { 0.5 * (I() + X(i)) } else { 0.5 * (I() - X(i)) }
}
```

Projectors are ordinary observables, so the probability of an outcome is an expectation
value. A product of single-qubit projectors on different qubits projects onto the
multi-qubit state:

```noev
let p11 = <ansatz(theta)| P(0, 1) * P(1, 1) |ansatz(theta)>   // probability of measuring 11
```

## 4. Output

### `print` and `eprint`

```noev
print("energy: {e:.3}")          // energy: -0.500
print("bits:", r, "count:", n)   // bits: 01 count: 3
eprint("warning: {k} retries")   // to standard error
```

`print(a, b, …)` writes its arguments to standard output, separated by spaces, followed by a
line break; `print()` writes an empty line. `eprint` does the same on standard error.
Arguments are any classical values, printed as [types.md](types.md) §7 describes.

Output is buffered: line by line when standard output is a terminal, in larger blocks when it
is a file or pipe, and always flushed when the program ends. Standard error is not buffered.

Reference definition, in terms of `std::io` below:

```noev
fn print(values: …) {           // variadic: not expressible in Noev yet
    let out = stdout().lock()
    out.write(/* values joined by " " */)
    out.write("\n")
}
```

Like the rest of the standard library, `print` and `eprint` are implemented natively in
v0.1. They are not allowed in a run that is repeated ([execution.md](execution.md) §3).

### `std::io` (Reserved)

Explicit control over output, as in Rust, for loops that write a lot and for programs that
must handle write errors. It needs modules and a `Result` type, so it comes after v0.1.

```noev
use std::io::stdout

let out = stdout().lock()        // one lock for the whole loop, not one per write
for i in 0..n {
    out.write("{i}: {values[i]}\n")
}
out.flush()
```

- `stdout()` and `stderr()` return handles; `lock()` holds the stream for its lifetime, so
  writes are not re-locked one by one and output from different threads cannot interleave.
- `write(s: String)` buffers; `flush()` writes the buffer out. Both report write errors as a
  `Result`.

## 5. Planned

Most of the standard library needs generics, so that one definition works for any register
size: the quantum Fourier transform, Grover iterations, arithmetic circuits, state
preparation, and common observables such as Ising and Heisenberg Hamiltonians. These arrive
with generics (`docs/PLAN.md` §11).
