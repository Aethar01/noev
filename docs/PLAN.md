# Noev — Project Plan (v0.1)

> **One language. Two computing paradigms.**
>
> Noev is a statically typed hybrid classical–quantum programming language built around
> resource ownership, memory safety, and compiler-managed quantum execution. Its goal is to
> abstract the complexity of quantum computing without hiding its physical constraints,
> allowing developers to write high-level hybrid programs while the compiler manages quantum
> lifetimes, operations, and execution boundaries.

## 1. Vision

Quantum programming today means thinking in qubits, gates, circuits, measurements and
hardware-specific execution. Noev aims to be to quantum programming what C was to assembly:
a higher-level abstraction that makes quantum resources as natural to work with as classical
memory, while the compiler enforces the laws of quantum mechanics.

Long-term, Noev provides:

- Native classical–quantum hybrid data structures.
- A quantum-aware ownership and borrowing system.
- Automatic management of quantum resource lifetimes.
- Safe, compiler-assisted uncomputation.
- A unified execution model for CPUs, GPUs and QPUs.
- A compiler that optimises resource allocation and circuit construction.

The programmer describes *what* to compute; Noev handles more of *how* to execute it safely.

## 2. Core abstraction: hybrid memory

```
Noev source      classical values + quantum resources + hybrid structures
     │
Noev compiler    type checking · ownership · uncomputation · optimisation · scheduling
     │
     ├── Classical runtime   CPU/GPU execution, values, memory, control flow
     └── Quantum runtime     qubit allocation, gates, measurement, reset
```

This is **unified logical memory, not physically shared RAM**. Classical variables can hold
references to quantum resources but cannot read or copy unknown quantum states.

## 3. Syntax at a glance

The full definition is in [`spec/`](../spec/README.md). Noev keeps Rust's ownership
model (owned values, shared and exclusive references, borrow checking) but has its own
syntax, so it is not mistaken for Rust.

```noev
struct Experiment {
    angle: Float
    state: Qubit

    fn rotate(mut self) -> mut Self {
        self.state.ry(self.angle)
        mut self
    }
}

fn run(angle: Float) -> Bit {
    let mut e = Experiment { angle, state: |0> }
    e.rotate().rotate()
    mv e.state.measure()
}
```

`angle` is classical; `state` is a quantum resource. The compiler tracks both in one
structure. `measure` takes ownership of the qubit (`mv`) and yields a classical `Bit`. The
programmer never picks physical qubit locations.

| Feature                      | Syntax                                          |
| ---------------------------- | ----------------------------------------------- |
| Create qubits                | Ket literals `\|0>`, `\|+>`, `\|01>`               |
| Measurement tests            | Bras: `<1\| mv q` (Z basis), `<+\| mv q` (X basis) → `Bool` |
| Expectation values           | `<ansatz(t)\| h \|ansatz(t)>` → `Float`         |
| Gates                        | Methods `q.h().s().ry(t)`, or free `h(q)`       |
| Ownership words              | `mv x` give away, `ref x` lend to read, `mut x` lend to change |
| Implicit lending             | `ref` always; `mut` for pure quantum values (`h(q)`); classical `bump(mut n)` stays written |
| Variables                    | `let`, `let mut` (classical data only; qubits can always be acted on) |
| Methods                      | Inside the `struct`; receivers `mv self`, `ref self`, `mut self` |
| Traits                       | `trait`, `impl Trait for Type` (reserved; generics assume values may be linear) |
| Paths                        | `Type::name`, `Float::PI`                       |
| Logic                        | `and`, `or`, `not`; bitwise `& \| ^ ~`          |
| Conversions                  | `Float(n)`; lossy ones are methods (`f.round()`) |
| Statements                   | Semicolons optional                             |
| Quantum control              | `ctrl a, b { c.x() }` (no `cx`, `ccx`, …)       |
| Classical control            | `if bit { … }`                                  |
| Reversible functions         | Inferred; `rev fn` promises it; inverted with `adj q.s()` |
| Compute / use / uncompute    | `with { … } do { … }` (reserved)                |

Quantum keywords are contextual, so `adj`, `rev`, `ctrl`, `mv` remain usable as variable
names. Every shortcut stands for a written-out form, which tools can display.

## 4. Ownership model

| Classical languages                          | Noev                                                                  |
| -------------------------------------------- | --------------------------------------------------------------------- |
| Rust prevents invalid memory access          | Noev also prevents invalid quantum resource access                    |
| Values can be copied according to their type | Unknown quantum states cannot be implicitly copied (no-cloning)       |
| Memory is freed when ownership ends          | Quantum cleanup must respect state and operation semantics            |
| Functions manipulate values                  | Quantum functions distinguish reversible and irreversible effects     |
| Compiler optimises memory and CPU use        | Compiler also optimises qubit reuse, circuit depth, resource lifetimes |

Rules the v0.1 checker must enforce — each of these is a compile-time error:

```noev
let q = |0>
let copy = q           // ERROR N001: qubits cannot be copied; write `mv q`
```

```noev
let q = |0>
let result = mv q.measure()
q.x()                  // ERROR N002: use after move (q consumed by measure)
```

Also: borrow errors (`swap(a, a)`, `ctrl a { a.x() }`, references outliving their value),
qubits that go out of scope without being consumed (measured, discarded, or returned),
irreversible operations in reversible code, and classical changes not written out. The full
list, with error codes N001–N020, is in
[`spec/ownership.md`](../spec/ownership.md).

Later goal: safe automatic uncomputation, where the compiler proves temporary quantum data
can be restored without damaging retained computation. Unlike classical GC, a quantum memory
manager cannot simply discard live states.

## 5. v0.1 MVP scope

Smallest system demonstrating hybrid data structures, quantum ownership and automatic
circuit generation.

| #  | Milestone                    | Description                                                                                | Status  |
| -- | ---------------------------- | ------------------------------------------------------------------------------------------ | ------- |
| 1  | Parser                       | Read `.nv` files, build an AST                                                             | ☑ Done |
| 2  | Basic types                  | `Int`, `Float`, `Bool`, `Bit`, `Qubit`, tuples, arrays, structs                            | ☑ Done |
| 3  | Ownership checker            | Rules N001–N022 in `spec/ownership.md` (`with`/`do` reserved)                              | ☑ Done |
| 4  | Quantum IR                   | Allocation, gates, measurement, classical dependencies                                     | ☑ Done |
| 5  | Circuits                     | Closed runs that are circuits (`spec/execution.md`) compiled to ketrs circuits and OpenQASM 3 | ☑ Done |
| 6  | Execution                    | Interpreter on ketrs, circuits sampled through ketrs, `noev run` | ☑ Done |
| 7  | Expectation values           | `<ψ\| H \|ψ>` with observables, exact on ketrs where possible                               | ☐ Todo |
| 8  | Backend interface            | The interpreter drives ketrs through the runtime's backend interface (`spec/runtime.md`), at circuit and gate level | ☑ Done |

**Priority:** build the type system and ownership checker (milestones 1–3) first, before any
simulator. A compiler that accepts valid quantum programs and rejects physically invalid
resource operations gives the project its technical identity from day one.

### Technology choices

| Component               | Choice                                |
| ----------------------- | ------------------------------------- |
| Compiler implementation | Rust                                  |
| Source extension        | `.nv` (provisional)                   |
| Lexer / parser          | Hand-written (recursive descent, Pratt for expressions) |
| Diagnostics             | `ariadne` (source-annotated errors)   |
| Type system             | Static, with linear quantum ownership |
| Export                  | OpenQASM 3; QIR later                 |
| Execution               | [`ketrs`](../../ketrs) (own simulator, path dependency), behind the runtime's backend interface |
| Native code (after v0.1) | Cranelift and LLVM, behind one code-generation interface (`spec/runtime.md`) |
| CLI                     | `noev`, arguments parsed with `clap`  |

- **Parser:** hand-written, for speed, full control over error recovery (report several
  errors per file, not just the first), and Noev's context-sensitive lexing (kets, line
  breaks). `ariadne` renders diagnostics; it only runs when there are errors, so it costs
  nothing on the fast path. Ownership errors are the product, so error quality matters from
  day one.
- **Performance is a feature.** The compiler must stay fast as it grows:
  - flat data: tokens and syntax-tree nodes in contiguous arrays addressed by `u32` indices,
    spans as `u32` byte offsets, identifiers interned; no per-node heap allocation;
  - one linear pass per phase where possible, no backtracking in the parser;
  - benchmarks for each phase from the start (lexing, parsing, checking), run on large
    generated programs, so regressions are caught when they happen;
  - avoid heavy generic or macro machinery in the compiler itself, to keep its own build
    fast.
- **Execution** (`spec/execution.md`): every call runs once, and repeated closed runs that
  are circuits are compiled to a `ketrs::Circuit` and sampled with `sample_circuit`, one
  simulation for all shots (ketrs shares the work before the first mid-circuit measurement).
  Runs that are not circuits are interpreted shot by shot on a `ketrs::Statevector`. The same
  circuit builder emits OpenQASM 3, and later QIR for hardware.
- **Compilation** (`spec/runtime.md`): programs compile to native code (Cranelift or LLVM,
  swappable) linked to the Noev runtime, which sends quantum work to a backend at circuit
  level (whole jobs, as hardware takes them today) or gate level (simulators, and future
  machines that control qubits gate by gate in real time). The interpreter uses the same
  runtime interface.

## 6. Repository layout

Noev is a language project, not just a Rust crate: the language is defined by its spec and
conformance tests, and the Rust compiler is one implementation of it. The compiler is a
Cargo workspace whose crates follow compiler phases, so dependencies only point downward
(syntax never depends on the runtime) and a future language server can reuse
`noev-syntax` + `noev-sema`.

```
noev/
├── Cargo.toml                 # workspace
├── README.md
├── spec/                      # the language definition
│   ├── grammar.md             #   lexical structure and syntax (EBNF)
│   ├── types.md               #   types, linearity, literals
│   ├── ownership.md           #   ownership rules and error codes
│   ├── core.md                #   intrinsics: gates, measurement, observables, maths
│   ├── std.md                 #   standard library, with reference definitions in Noev
│   ├── runtime.md             #   code generators, the runtime and quantum backends (design)
│   ├── modules.md             #   modules, imports, visibility, packages (reserved)
│   └── interop.md             #   calling into and out of Noev (reserved)
├── crates/
│   ├── noev-syntax/           # tokens, lexer, parser, AST, spans
│   ├── noev-sema/             # name resolution, type checker, ownership checker
│   ├── noev-ir/               # quantum IR, lowering from AST
│   ├── noev-runtime/          # interpreter, backend interface (ketrs), OpenQASM 3 output
│   └── noev/                  # CLI binary: check / build / run, diagnostics rendering
├── examples/                  # hello.nv, bell.nv, hybrid.nv
├── tests/
│   ├── ui/                    # .nv programs + expected diagnostics (compile-fail and pass)
│   └── run/                   # .nv programs + expected measurement distributions
└── docs/                      # PLAN.md, design notes
```

Later, not v0.1: `std/` (the standard library as Noev source; until then it is implemented
natively, see `spec/std.md`), `editors/` (tree-sitter grammar,
VS Code syntax), `noev-lsp`.

Long-term goal: a self-hosting compiler written in Noev. The Rust implementation is the
bootstrap compiler, so keep the spec and the `tests/` suite as the authority on behaviour,
not Rust-specific details. A self-hosted compiler must pass the same suite.

CLI:

```
noev check examples/bell.nv
noev build examples/bell.nv --emit=plan
noev run   examples/bell.nv --circuits=out/
```

## 7. First target program: Bell pair

```noev
fn bell_pair() -> (Bit, Bit) {
    let a = |+>
    let b = |0>
    ctrl a { b.x() }
    (mv a.measure(), mv b.measure())
}
```

Expected over many shots: ~50% `00`, ~50% `11`.

**First milestone:** parse this, verify ownership, generate a circuit, execute it on a
simulator. Then add a hybrid struct with a classical parameter controlling a quantum
operation (the `Experiment` example) — that demonstrates the language's core idea, not just
gate generation.

## 8. Differentiation

Prior art: [Guppy](https://docs.quantinuum.com/guppy/language_guide/ownership.html) (quantum
ownership, linear types), [Silq](https://silq.ethz.ch/) (automatic uncomputation). Noev's
contribution should not just be quantum variables or a borrow checker. Research questions:

1. **Can classical and quantum resources feel like one programming model?** Hybrid structs,
   shared function syntax, explicit but ergonomic classical↔quantum transitions.
2. **Can the compiler manage quantum lifetimes better than programmers?** Analyse where
   quantum values are created, entangled, measured, reset or safely uncomputed; minimise
   resource usage.
3. **Can the compiler optimise the complete hybrid computation?** Optimise classical
   execution, quantum operations, communication and scheduling together under hardware
   constraints.

## 9. Explicitly out of scope for now

- Automatic CPU/GPU/QPU placement
- Complex automatic uncomputation
- Real QPU integrations
- Hardware-specific optimisers
- Explicit lifetimes, generics and references in struct fields (v0.1 borrow-checks references with elided lifetimes only)
- A general-purpose standard library
- Interop with other languages (planned in §11)

## 10. Design decisions

Guiding principle for v0.1: **start strict, relax later.** Turning an error into an accepted
program never breaks existing code; the reverse does.

| Decision            | Choice                                                    | Status   |
| ------------------- | --------------------------------------------------------- | -------- |
| Parser              | Hand-written lexer and parser, diagnostics with `ariadne` | Decided  |
| Simulator           | `ketrs`, driven directly from IR                          | Decided  |
| Project structure   | Language project: `spec/` + Cargo workspace (§6)          | Decided  |
| Standalone language | Own compiler and syntax; not a Rust superset or rustc fork (below) | Decided  |
| References          | Rust's model, spelled `ref`/`mut`; lexical lifetimes in v0.1 (below) | Decided  |
| Methods and traits  | Methods inside structs; traits via `impl Trait for Type` (after v0.1) | Decided  |
| Syntax              | Own syntax on Rust's ownership model (§3, `spec/`)        | Decided  |
| Shortcuts           | Implicit `ref`, implicit `mut` for pure quantum values, inferred `rev` | Decided  |
| Array indices       | Compile-time proof that indices differ and are in range (`spec/ownership.md` §10) | Decided  |
| `assert`, `unsafe`  | `assert` adds runtime-checked facts; `unsafe` trusts uncheckable claims (`release`) | Decided  |
| Execution model     | Calls run once; closed runs repeat as shots; a one-sentence rule decides circuit vs shot by shot; `circuit fn` promises it (`spec/execution.md`) | Decided  |
| Output              | `noev run` runs `main` once; output is what the program prints (`print`, `eprint`); `std::io` reserved | Decided  |
| Strings, formatting | `String` with value semantics; `{expr:spec}` interpolation checked at compile time | Decided  |
| Sampling            | `sample n { … } -> Distribution<T>` in the language, no CLI command: a block that runs `n` times, so every call still runs once where it is written | Decided  |
| Generic syntax      | Angle brackets: `Distribution<T>` | Decided  |
| Compilation         | Native code linked to a Noev runtime; code generators (Cranelift, LLVM) swappable behind the IR; QIR for export and linking (`spec/runtime.md`) | Decided  |
| Classical control   | `if`s on measurement results whose branches only apply gates become `if`s in the circuit; measurement-dependent loops and richer branches run shot by shot for now, reported | Decided  |
| Quantum backends    | One interface, two levels: circuit (jobs) and gate (operation by operation); ketrs first | Decided  |
| Unconsumed qubits   | Compile error now; goal is implicit free when provably \|0⟩ | Decided  |

### References: Rust's model, Noev's spelling

Noev has Rust's references, written `ref T` and `mut T`, created with `ref x` / `mut x`, and
borrow-checked. Gates are `mut self` methods returning `mut Self`, so chaining
(`q.h().s()`) works as Rust's builder pattern does, and `let r = q.h()` stores the lend.
Reference variables stand for the value they refer to, so there is no `*`.

v0.1 uses lexical lifetimes (a stored lend lasts to the end of its block) and Rust's
elision rules for signatures. Non-lexical lifetimes come later; they only accept more
programs, so nothing breaks. Explicit lifetimes and references in structs need generics.

Differences from Rust:

- **Moves are written `mv`.** A variable only becomes unusable at a visible `mv`.
- **Qubits are linear**: they cannot be dropped (N003).
- **Implicit lending.** `ref` can always be left out, and `mut` can be left out for pure
  quantum values (`h(q)`), because a bare name can never mean a move. Classical changes stay
  written (`bump(mut n)`).
- **`let mut` is about classical data.** Qubits can always be acted on.
- **A shared `ref Qubit` means no operation is applied through it**, not that the state
  can't change: a `ctrl` control can pick up a phase (phase kickback).

History: earlier drafts had call-scoped borrows with no reference types, then Rust's exact
syntax. The current form keeps Rust's semantics with Noev's own spelling.

### Standalone, not a Rust superset

Considered: Noev as Rust plus quantum syntax, compiled through `rustc` (via a translation step
and a `rustc` driver, or a fork of `rustc`). Rejected on 2026-11-05:

- Rust's standard library and generic code assume every value can be dropped, which fights
  Noev's core guarantee that qubits must be consumed. A fork gives access to the compiler,
  not a solution: Rust itself has not settled must-move types.
- A fork would touch the most actively changing parts of `rustc` and break rust-analyzer,
  rustfmt and Clippy. The driver route rules out type-aware syntax such as implicit borrows of
  qubits.
- Noev needs full control of its semantics for reversibility, uncomputation and whole-program
  hybrid optimisation.

Noev keeps Rust's ownership model, but its syntax must be clearly its own, so it is not
mistaken for Rust.

### Unconsumed qubits: error

If a qubit is still owned at the end of its scope, that is an error (N003). The fix-it
suggests `measure(mv q)`, `discard(mv q)` (reset and release), or returning it. Rejected
alternatives:

- **Implicit discard at end of scope.** If `q` is entangled with a qubit that is kept,
  discarding it is physically a measurement, and it silently collapses the kept qubit. That
  hides the physics, which is exactly what Noev promises not to do.
- **Implicit free when the qubit is provably |0⟩.** This is the right long-term direction:
  it is the entry point to compiler-managed uncomputation (research question 2). It needs
  state analysis, so it is deferred. Because v0.1 rejects these programs, accepting them
  later breaks nothing. **This is the stated goal**: v0.1 ships the strict rule, and the
  ownership checker should track enough per-qubit state (e.g. "untouched since
  allocation") that this analysis can be added without redesign.

### Toolchain

Rust 1.99 stable (≥ 1.88, edition 2024, matching ketrs). rustup lives in
`~/.local/share/rustup` (`RUSTUP_HOME`, set by the fish config). Non-fish shells that
don't set it have no toolchain, so set `RUSTUP_HOME` when running cargo from bash.

## 11. After v0.1

Planned once milestones 1–7 are done, roughly in this order.

### Compilation (`spec/runtime.md`)

1. **Runtime C ABI**, shaped like QIR's, with the interpreter moved onto it.
2. **Cranelift code generator** and `noev build --emit=binary|object`; compiled programs
   tested against the interpreter.
3. **QIR export** (`--emit=qir`): base profile for circuits, adaptive profile for classical
   control.
4. **LLVM code generator**, optionally emitting QIR calls so programs link to QIR runtimes.
5. **Hardware backends** at circuit level, and compile-time checks against a target.

### Interop (`spec/interop.md`)

Only classical values cross the boundary; the host never holds a qubit.

1. **Rust embedding API with host functions.** A Rust program loads a Noev program, calls its
   functions, and provides `extern fn`s, checked against their declarations when the program
   is loaded. Small, because the runtime is already a Rust library, and safe, so no `unsafe`.
2. **Python bindings** on top of it (PyO3, as in `ketrs-py`). Reaches most of the quantum
   audience: Python optimisers driving Noev expectation values.
3. **C, both ways:** `extern "C"` imports called inside `unsafe`, and a `noev.h` API for C and
   C++ hosts. Imports need native compilation or `libffi`; easiest after native code
   generation.
4. **Inline OpenQASM** for hardware-specific gates (`unsafe`).

Hardware targets will need a rule for foreign calls made while qubits are alive (they run on
control electronics with tight latency limits); see `spec/interop.md` §5.

### Language

- Modules and packages (`spec/modules.md`): files are modules, `use` imports, `pub`
  visibility, `noev.toml` packages. Needs per-file span offsets first.
- Traits and generics (`spec/types.md` §6), with generic code assuming values may be linear.
- `with`/`do` compute–use–uncompute blocks, and automatic release of qubits proven to be in
  \|0⟩ (`spec/ownership.md` §9).
- Non-lexical lifetimes, explicit lifetimes, and references in structs.

