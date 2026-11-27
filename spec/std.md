# Standard library

Everything that comes with Noev but is not an intrinsic ([core.md](core.md)). Each item is
specified by a **reference definition** written in Noev, using only core and other
standard-library items.

**Reference semantics.** An implementation may provide standard-library items natively or
as Noev source, provided every item behaves identically to its reference definition: same
signature, same ownership rules, same reversibility, and same results.

## 1. What is in scope

Every item in [core.md](core.md) is in the **prelude**, the names every module has without
imports, and so are most items in this document. The others are in modules of the standard
library, under `std::`, and need a `use`: each says which in its heading. A module's own
names, and its imports, take precedence over the prelude's ([modules.md](modules.md) §6).

## 2. Qubits

### `discard`

Resets a qubit and releases it. Consumes the qubit; not reversible.

```noev
fn discard(q: Qubit) {
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
fn measure_in(q: Qubit, basis: Basis) -> Bit {
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

### `State` (`std::state`)

The six basis states a ket or bra label names, as a value, so a label can be computed
(`|{s}>`, [types.md](types.md) §3).

```noev
/// One qubit's basis state: the labels `0 1 + - (+i) (-i)`.
enum State { Zero, One, Plus, Minus, PlusI, MinusI }
```

As with `Basis`, a program's own type called `State` takes the name, and kets still accept
the standard one.

### `amplitudes` (`std::debug`; backends that can read the state)

For debugging: the amplitudes of a register's state, on a backend with the capability to
read it, such as a simulator ([runtime.md](runtime.md) §3). A program that calls it is
rejected for a target without that capability.

```noev
use std::debug::amplitudes

let r = |+0>
ctrl r[0] { r[1].x() }
print(amplitudes(r))     // [0.7071…+0.0i, 0.0+0.0i, 0.0+0.0i, 0.7071…+0.0i]
```

`amplitudes(r)` lends `r`, a `Qubit` or `[Qubit; N]` with N at most 20, and gives a
`[Complex; 2^N]` over its basis states in the order kets read them: \|00⟩, \|01⟩, \|10⟩,
\|11⟩, with `r[0]` the left label. It changes nothing. A register entangled with other qubits
has no state of its own, so asking for one is a runtime error. The overall phase is chosen so that the first amplitude that is not zero
is real and positive; only the relative phases between amplitudes mean anything physically.

It is not reversible, since under `adj` a function's gates are collected rather than run, and
a `sample` that reads amplitudes runs shot by shot.

## 3. Classical post-processing

### `fft` and `ifft` (`std::fourier`)

The discrete Fourier transform of a `[Complex; N]` or `[c32; N]`, as NumPy computes it:

```noev
fn fft(x: [Complex; N]) -> [Complex; N]     // X[k] = Σ x[n] e^(−2πi·kn/N)
fn ifft(x: [Complex; N]) -> [Complex; N]    // x[n] = (1/N) Σ X[k] e^(2πi·kn/N)
```

`ifft(fft(x))` is `x`, to rounding. Lengths that are powers of two take N log N steps, others
N². Phase estimation reads its phase from the inverse transform of the measured register's
amplitudes or counts; the quantum Fourier transform itself is a circuit of gates.

## 4. Observables

### `P` and `PX` (`std::observables`)

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
use std::observables::P

let p11 = <ansatz(theta)| P(0, 1) * P(1, 1) |ansatz(theta)>   // probability of measuring 11
```

## 5. Output

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

`print` and `eprint` are not allowed in a run that is repeated ([execution.md](execution.md) §3).

### `std::io`

Explicit control over output and standard input, for loops that write a lot and for programs
and backends that must handle I/O errors.

```noev
use std::io::{stdout, stderr, stdin}

let out = stdout().lock()        // one lock for the whole loop, not one per write
for i in 0..n {
    out.write("{i}: {values[i]}\n")
}
out.flush()
```

- `stdout() -> IoStream`, `stderr() -> IoStream`, and `stdin() -> Stdin` return stream
  handles. `IoStream::lock(ref self) -> IoStream` returns a handle to the locked stream.
- `IoStream::write(ref self, s: String) -> Result<(), String>` writes `s`;
  `IoStream::flush(ref self) -> Result<(), String>` flushes buffered output.
- `Stdin::read_line(ref self) -> Result<String, String>` reads one line;
  `Stdin::read_to_string(ref self) -> Result<String, String>` reads until EOF.
- `IoStream` and `Stdin` are affine handles (not implicitly copied; dropped freely). All
  `std::io` operations are irreversible (N013).

## 6. Classical collections, strings, and OS interfaces for backends

Backends can be written in Noev ([runtime.md](runtime.md) §3), so the standard library
provides the classical types and OS modules they need.

### `Vec<T>` (prelude)

A growable heap-allocated array of classical (non-linear) elements `T`. `Vec<T>` is affine:
it is moved with `mv`, lent with `ref`/`mut`, dropped automatically at scope exit (no N003),
and never implicitly copied (use `.clone()` for an explicit copy).

- Construction: `Vec::new() -> Vec<T>` (with element type inferred from context, `Vec::new::<T>()`, or `Vec::<T>::new()`), `Vec::with_capacity(cap: Int) -> Vec<T>`.
- Reading (`ref self`): `len() -> Int`, `is_empty() -> Bool`, `get(index: Int) -> Option<T>`,
  `clone() -> Vec<T>`, indexing `v[i] -> T` (bounds-checked at run time), `==` / `!=`, and
  printing/formatting (`[a, b, c]`).
- Mutation (`mut self`, requires `let mut`): `push(value: T)`, `pop() -> Option<T>`,
  `set(index: Int, value: T)` and `v[i] = value` (bounds-checked at run time), `clear()`,
  `extend(other: ref Vec<T>)`.

### `String` methods (prelude)

- Inspection: `len() -> Int` (UTF-8 byte length), `is_empty() -> Bool`,
  `contains(pat: String) -> Bool`, `starts_with(pat: String) -> Bool`,
  `ends_with(pat: String) -> Bool`, `find(pat: String) -> Option<Int>`.
- Transformation: `trim() -> String`, `replace(from: String, to: String) -> String`,
  `substring(start: Int, end: Int) -> Option<String>`, `to_uppercase() -> String`,
  `to_lowercase() -> String`, `repeat(n: Int) -> String`, and `+` concatenation (`a + b`).
- Splitting, joining, bytes and parsing: `split(sep: String) -> Vec<String>`,
  `lines() -> Vec<String>`, `String::join(parts: ref Vec<String>, sep: String) -> String`,
  `as_bytes() -> Vec<u8>`, `String::from_utf8(bytes: ref Vec<u8>) -> Result<String, String>`,
  `parse_int() -> Result<Int, String>`, `parse_float() -> Result<Float, String>`.

### `std::env`

Environment and process inspection (irreversible, N013):

- `var(key: String) -> Option<String>` — reads an environment variable.
- `args() -> Vec<String>` — returns the program's command-line arguments.
- `current_dir() -> Result<String, String>` — returns the current working directory.

### `std::fs`

File-system access for configuration, credentials, and circuit artefacts (irreversible, N013):

- `read_to_string(path: String) -> Result<String, String>`
- `read(path: String) -> Result<Vec<u8>, String>`
- `write(path: String, content: String) -> Result<(), String>`
- `write_bytes(path: String, data: ref Vec<u8>) -> Result<(), String>`
- `exists(path: String) -> Bool`
- `create_dir_all(path: String) -> Result<(), String>`
- `remove_file(path: String) -> Result<(), String>`

### `std::net`

Plain TCP sockets (irreversible, N013); TLS/HTTPS lives in external packages binding a
maintained C library so Noev never maintains TLS:

- `TcpStream` (affine handle): `TcpStream::connect(addr: String) -> Result<TcpStream, String>`,
  `read(ref self, max_bytes: Int) -> Result<Vec<u8>, String>`,
  `read_to_string(ref self) -> Result<String, String>`,
  `write(ref self, data: ref Vec<u8>) -> Result<Int, String>`,
  `write_str(ref self, s: String) -> Result<Int, String>`,
  `flush(ref self) -> Result<(), String>`,
  `shutdown(ref self) -> Result<(), String>`,
  `close(mv self)`.
- `TcpListener` (affine handle): `TcpListener::bind(addr: String) -> Result<TcpListener, String>`,
  `local_addr(ref self) -> Result<String, String>`,
  `accept(ref self) -> Result<TcpStream, String>`,
  `close(mv self)`.

## 7. Backend interface and future libraries

`std::backend` (`Backend`, `Target`, `Level`, `Capabilities`, `Circuit`, `Instruction`,
`Op`, `GateKind`, `Shots`, `ShotCount`, `BackendError`) is specified in
[runtime.md](runtime.md) §3–§5. Higher-level generic quantum algorithm libraries (QFT,
Grover, arithmetic circuits, Hamiltonians) build on these primitives.
