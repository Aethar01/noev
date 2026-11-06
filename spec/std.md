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

## 4. Planned

Most of the standard library needs generics, so that one definition works for any register
size: the quantum Fourier transform, Grover iterations, arithmetic circuits, state
preparation, and common observables such as Ising and Heisenberg Hamiltonians. These arrive
with generics (`docs/PLAN.md` §11).
