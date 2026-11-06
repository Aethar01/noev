# Noev

**One language. Two computing paradigms.**

Noev is a statically typed hybrid classical–quantum programming language. Classical and
quantum code are written together in one language, with Rust's ownership model making it
impossible to copy, forget or reuse a qubit, and the compiler deciding what runs as a quantum
circuit.

```noev
fn bell() -> (Bit, Bit) {
    let a = |+>
    let b = |0>
    ctrl a { b.x() }
    (mv a.measure(), mv b.measure())
}

fn main() {
    let d = sample 1000 { bell() }
    print(d)
    print("P(11) = {d.probability((1, 1)):.3}")
}
```

```
$ noev run examples/bell.nv
00   507   50.7%
11   493   49.3%
P(11) = 0.493
```

Status: 0.1, the first release. The language is specified in [`spec/`](spec/), and this
compiler implements all of it apart from the parts marked *Reserved* there (modules, traits
and generics, `with`/`do`, interop). Expect changes before 1.0.

## What it does

- **Qubits are owned.** A qubit is a linear value: it must be measured, discarded or returned
  exactly once, and can't be copied. `ref` and `mut` lend it, as in Rust. Breaking a rule is
  a compile error with its own code (N001–N022, [`spec/ownership.md`](spec/ownership.md)).
- **Every call runs once.** Code reads in order, quantum or not. Repeating a run is explicit:
  `sample n { … }` runs a block `n` times and gives a `Distribution`, and `<ψ| H |ψ>` is an
  expectation value, estimated `within` a precision you choose.
- **The compiler finds the circuits.** A run whose measurement results are only used in
  conditions, logic or its result is a circuit, including `if`s and `while`s on measurement
  results (classical control). Circuits are simulated once for all shots, and sent as one
  job to hardware. `noev build --emit=plan` says how each run executes, and why
  ([`spec/execution.md`](spec/execution.md)).
- **Bras and kets.** `|0+1>` makes qubits, `<+| mv q` measures in a basis, `ctrl a { … }`
  controls a block, `adj f(q)` inverts a reversible function.
- **Backends.** Programs run on the [ketrs](https://github.com/Aethar01/ketrs) simulator by
  default, through a backend interface designed for hardware too
  ([`spec/runtime.md`](spec/runtime.md)). `noev export` writes a program's circuits as
  OpenQASM 3 without simulating anything.

## Build

Noev needs Rust 1.88 or later.

```
git clone https://github.com/Aethar01/noev
cd noev
cargo build --release
```

The compiler is then `target/release/noev`; the examples below call it `noev`.

The [ketrs](https://crates.io/crates/ketrs) simulator is built in through the `ketrs`
feature, on by default. `cargo build --release --no-default-features` builds Noev without a
simulator: it still checks, builds and exports programs, but `noev run` needs
`--backend=export`.

## Use

```
noev check prog.nv              # types and ownership
noev run prog.nv                # run main; output is what it prints
noev run prog.nv --seed 7       # reproduce a run
noev build prog.nv --emit=plan  # how each run executes: circuit or shot by shot, and why
noev build prog.nv --emit=ir    # the compiler's intermediate representation
noev export prog.nv -o out/     # every circuit, as OpenQASM 3, without running anything
```

The [`examples/`](examples/) show the language: a Bell pair, a hybrid struct holding an angle
and a qubit, a table of energies from expectation values, a 16-qubit quantum Fourier
transform, and teleportation.

## Editors

[`editors/`](editors/) has a tree-sitter grammar (highlighting, indentation, folding) and
Neovim support, and `noev lsp` is a language server for any editor: errors as you type, types
on hover, go to definition and document symbols. See
[`editors/nvim/README.md`](editors/nvim/README.md) for setting up Neovim.

## Repository

- [`spec/`](spec/): the language definition. The compiler implements it; behaviour changes
  start there.
- [`tests/ui/`](tests/ui/) and [`tests/run/`](tests/run/): the conformance suite, checked only
  through the `noev` command line, so any implementation can use it.
- [`crates/`](crates/): the compiler, one crate per phase: `noev-syntax` (lexer, parser),
  `noev-sema` (types, ownership, circuits), `noev-ir`, `noev-runtime` (interpreter, backends,
  OpenQASM output) and `noev` (the command line).
- [`docs/PLAN.md`](docs/PLAN.md): the plan, decisions and roadmap.

`cargo test --workspace` runs everything, and `cargo bench -p noev-syntax`, `-p noev-sema` and
`-p noev-ir` measure each phase.

## Licence

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your
option. This covers the compiler, the language specification in `spec/`, and the
conformance tests in `tests/`.
