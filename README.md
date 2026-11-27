<div align="center">

# Noev

**One language. Two computing paradigms.**

A statically typed hybrid classical-quantum programming language. Linear ownership prevents copying, aliasing, or forgetting a qubit at compile time, and the compiler extracts quantum circuits automatically.

[![CI](https://github.com/Aethar01/noev/actions/workflows/ci.yml/badge.svg)](https://github.com/Aethar01/noev/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Aethar01/noev?include_prereleases&label=release)](https://github.com/Aethar01/noev/releases)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org)
[![Licence](https://img.shields.io/badge/licence-MIT%20%2F%20Apache--2.0-blue.svg)](#licence)

[Docs](https://aethar01.github.io/noev/) · [Tour](#language-tour) · [Install](#install) · [CLI](#cli) · [Interop](#interop) · [Examples](#examples) · [Spec](spec/README.md)

</div>

---

```noev
fn bell_pair() -> (Bit, Bit) {
    let a = |+>
    let b = |0>
    ctrl a { b.x() }
    (mv a.measure(), mv b.measure())
}

fn main() {
    let d = sample 1000 { bell_pair() }
    print(d)
    print("P(11) = {d.probability((1, 1)):.3}, most likely: {d.most_likely()}")
}
```

```text
$ noev run examples/bell.nv --seed 7
00   526   52.6%
11   474   47.4%
P(11) = 0.474, most likely: 00
```

> **Status: 0.5.** The compiler implements the language specification in [`spec/`](spec/README.md): linear quantum ownership, generics and traits, `with`/`do` uncomputation, non-lexical lifetimes, modules and packages, Cranelift and LLVM native code generation, OpenQASM 3 and QIR 1.0 export, Rust/Python/C interop, plug-in backends, a built-in test runner, and an LSP server. Expect changes before 1.0.

---

## Overview

- **Linear qubit ownership.** Qubits cannot be copied, aliased, or silently dropped. Moves are written `mv`, lends use `ref` and `mut`, and rule violations are compile errors with codes `N001` to `N024` ([`spec/ownership.md`](spec/ownership.md)).
- **Automatic circuit extraction.** Every call runs once; repeated runs are explicit with `sample n { … }`. Runs whose measurement results only drive conditions, logic, or return values (including `if` and `while` classical control) compile to one circuit ([`spec/execution.md`](spec/execution.md)).
- **Quantum notation.** Kets (`|+>`, `|0{b}>`), bras (`<+| mv q`, `<bell()| mv pair`), expectation values (`<ψ| H |ψ> within ε`), amplitudes (`<φ|ψ>`), controlled blocks (`ctrl a, b { … }`), adjoints (`adj f(q)`), and uncomputation (`with { … } do { … }`).
- **Compiler and backends.** Interpret on [`ketrs`](https://github.com/Aethar01/ketrs), compile to native binaries with Cranelift or LLVM, export OpenQASM 3 or QIR 1.0 (Base and Adaptive profiles), or target plug-in backends with compile-time capability checks ([`spec/runtime.md`](spec/runtime.md)).
- **Interop.** Embed Noev in Rust, Python, or C/C++, or call host functions, `extern "C"` libraries, and inline OpenQASM from Noev. Only classical values cross the boundary ([`spec/interop.md`](spec/interop.md)).

---

## Language Tour

### 1. Compile-time ownership checks

Copying a qubit, using it after measurement, aliasing control and target, or forgetting to consume a live qubit are all compile-time errors:

```noev
fn bad() {
    let a = |+>
    ctrl a { a.x() }           // control and target are the same qubit
    let b = a                  // implicit copy of a qubit
}
```

```text
error[N005]: `a` is a control of this block
 --> example.nv:3:14
  |
3 |     ctrl a { a.x() }
  |          -   ^
  |          lent here

error[N001]: `a` holds qubits, which cannot be copied
 --> example.nv:4:13
  |
4 |     let b = a
  |             ^
  = help: write `mv a` to move it
```

Leaving an owned qubit unconsumed at scope exit is `N003`. Qubits proven to be in `|0>` (untouched or after `reset`) release automatically.

### 2. Hybrid structs and expectation values

Classical values and qubits live in the same structs, generics, and functions. Expectation values use bra-ket syntax directly:

```noev
fn ansatz(theta: Float) -> [Qubit; 2] {
    let r = |00>
    r[0].ry(theta)
    ctrl r[0] { r[1].x() }
    mv r
}

fn energy(theta: Float) -> Float {
    let h = -1.0 * Z(0) * Z(1) + 0.5 * (Z(0) + Z(1)) + 0.5 * X(0) * X(1)
    <ansatz(theta)| h |ansatz(theta)>
}
```

### 3. Reversible functions and `with`/`do` uncomputation

`with { … } do { … }` computes into temporary ancillas, lends them `ref` inside `do`, inverts `with`, and releases the ancillas automatically. It composes with `ctrl` and `adj`:

```noev
rev fn cccx(a: ref Qubit, b: ref Qubit, c: ref Qubit, target: mut Qubit) {
    with {
        let anc = |0>
        ctrl a, b { anc.x() }
    } do {
        ctrl anc, c { target.x() }
    }
}
```

### 4. Circuit detection with classical control

Mid-circuit measurements feeding `if` or `while` stay inside one circuit. `noev build --emit=plan` reports how each run executes and why:

```text
$ noev build examples/teleport.nv --emit=plan
sample (line 41)  circuit: sampled from one simulation
fn correct        circuit
fn teleported     circuit
```

---

## Architecture

```text
  source (.nv)
       │
       ▼
  noev-syntax ──▶ noev-sema (types · linear ownership · circuit analysis · targets)
                       │
                       ▼
                    noev-ir
                       │
       ┌───────────────┼──────────────────┬────────────────────┐
       ▼               ▼                  ▼                    ▼
  Interpreter    Native codegen        Export            Plug-in backends
  (ketrs sim)    Cranelift / LLVM   OpenQASM 3 / QIR   (Noev · Rust · C ABI)
```

---

## Install

### Prebuilt binaries

Download archives for Linux (`x86_64`, `aarch64`), macOS (`arm64`, `x86_64`), and Windows (`x86_64`) from [GitHub Releases](https://github.com/Aethar01/noev/releases). Each archive contains the `noev` binary, runtime libraries, and C headers (`noev.h`, `noev_backend.h`).

### Build from source

Requires **Rust 1.88 or later**:

```bash
git clone https://github.com/Aethar01/noev
cd noev
cargo build --release
```

The compiler binary is `target/release/noev`.

> **Optional features:** Default builds include the [`ketrs`](https://crates.io/crates/ketrs) simulator (`ketrs`) and both native code generators (`cranelift`, `llvm`). Build without optional dependencies with `cargo build --release --no-default-features`. Type checking, circuit planning, and OpenQASM 3 / QIR export work without a simulator; `noev run` then uses `--backend=export`.

---

## CLI

| Command | Description |
| --- | --- |
| `noev check prog.nv` | Check types, ownership, and target capabilities |
| `noev run prog.nv [--seed N]` | Run `main` (reproducible with `--seed`, or choose `--backend`) |
| `noev test [FILTER] [--compile]` | Run `test fn` suites in a file, package, or directory |
| `noev build prog.nv` | Compile to a native binary (`--codegen=cranelift\|llvm`, `--qir-runtime`) |
| `noev build prog.nv --emit=plan` | Report whether each run executes as a circuit or shot by shot, and why |
| `noev build prog.nv --emit=qir` | Emit QIR 1.0 LLVM IR (Base or Adaptive profile) |
| `noev export prog.nv -o out/` | Export circuits without simulating (`--format=qasm3\|qir`) |
| `noev target list\|show\|fetch` | Inspect or cache backend target descriptions |
| `noev lsp` | Start the Language Server Protocol server over stdio |

---

## Interop

Only classical values cross language boundaries; qubits stay inside Noev's ownership system. See [`spec/interop.md`](spec/interop.md) for the full reference.

<details>
<summary><strong>Python</strong> (PyO3 / maturin)</summary>

```python
import noev

program = noev.Program.compile_file("examples/energy.nv")
rt = noev.Runtime(program, seed=42)
print(rt.call("energy", 0.5))
```

</details>

<details>
<summary><strong>Rust</strong> (<code>noev</code> crate)</summary>

```rust
let program = noev::Program::compile_file("vqe.nv")?;
let mut runtime = noev::Runtime::new(&program)
    .host_fn("minimize_step", |energy: f64, theta: f64| optimiser.step(energy, theta));
let energy: f64 = runtime.call("vqe", ())?;
```

</details>

<details>
<summary><strong>C / C++</strong> (<code>extern "C"</code> and <code>include/noev.h</code>)</summary>

- **Call C from Noev:** declare `extern "C"` functions, `extern "C" struct` layouts, and `extern "C" fn` callbacks; call inside `unsafe`.
- **Embed Noev in C/C++:** link against `libnoev_c.a` and include [`include/noev.h`](include/noev.h).
- **Hardware plug-ins:** implement [`include/noev_backend.h`](include/noev_backend.h) or `std::backend::Backend` in Noev.

</details>

---

## Examples

Run any example with `noev run examples/<name>.nv`:

| Example | Description |
| --- | --- |
| [`examples/bell.nv`](examples/bell.nv) | Bell pair, `ctrl`, `sample`, and `Distribution` queries |
| [`examples/hybrid.nv`](examples/hybrid.nv) | Hybrid `struct` holding a classical `Float` angle and an owned `Qubit` |
| [`examples/energy.nv`](examples/energy.nv) | Parameterised ansatz and `<ψ\| H \|ψ>` expectation values |
| [`examples/qft.nv`](examples/qft.nv) | 16-qubit Quantum Fourier Transform sampled from one simulation |
| [`examples/teleport.nv`](examples/teleport.nv) | Quantum teleportation with mid-circuit measurement and classical `if` control |
| [`examples/clone.nv`](examples/clone.nv) | Encrypted quantum cloning where a single-use decryption key is ownership |

---

## Editors

- **Language server (`noev lsp`):** diagnostics with error codes as you type, hover information (types, docs, circuit and reversibility status), go to definition, and document symbols.
- **Tree-sitter and Neovim:** [`editors/tree-sitter-noev`](editors/tree-sitter-noev) provides highlighting, indentation, and folding; see [`editors/nvim/README.md`](editors/nvim/README.md) for Neovim setup.

---

## Specification and Repository

The language is defined by [`spec/`](spec/README.md) and its CLI conformance suite in [`tests/`](tests/); the Rust compiler is one implementation of it.

| Path | Contents |
| --- | --- |
| [`spec/`](spec/README.md) | Language specification (`grammar`, `types`, `ownership`, `core`, `std`, `execution`, `runtime`, `modules`, `interop`, `testing`) |
| [`crates/noev-syntax`](crates/noev-syntax) | Hand-written lexer, recursive-descent and Pratt parser, flat AST, spans |
| [`crates/noev-sema`](crates/noev-sema) | Name resolution, type checker, linear ownership and NLL checker, circuit and target analysis |
| [`crates/noev-ir`](crates/noev-ir) | Hybrid classical-quantum IR and AST lowering |
| [`crates/noev-codegen`](crates/noev-codegen) | Cranelift and LLVM native code generators |
| [`crates/noev-runtime`](crates/noev-runtime) | Runtime C ABI, interpreter, `ketrs` and plug-in backends, OpenQASM 3 and QIR 1.0 export |
| [`crates/noev-c`](crates/noev-c) · [`crates/noev-py`](crates/noev-py) | C/C++ embedding library and Python bindings |
| [`crates/noev`](crates/noev) | CLI binary, Rust embedding API, LSP server, diagnostic renderer |
| [`tests/ui/`](tests/ui/) · [`tests/run/`](tests/run/) | Compile-fail diagnostics (`N001`–`N024`) and executable conformance tests |
| [`docs/PLAN.md`](docs/PLAN.md) · [`CHANGELOG.md`](CHANGELOG.md) | Design decisions, roadmap, and release history |

### Development

```bash
cargo test --workspace                               # full test suite
cargo test --workspace --no-default-features         # build and test without simulator/codegen
cargo bench -p noev-syntax -p noev-sema -p noev-ir   # phase benchmarks
```

---

## Licence

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option. This covers the compiler, the language specification in `spec/`, and the conformance tests in `tests/`.
