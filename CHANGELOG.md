# Changelog

Notable changes to Noev, the language and this compiler. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/), with breaking changes allowed in minor versions
before 1.0.

## [Unreleased]

### Language

- Enums with plain variants (`enum Basis { X, Y, Z }`), with methods inside, compared with
  `==`, and printed as `Basis::X`.
- Variants with data, as values (`Outcome::Success(Bit)`) or named fields
  (`Outcome::Retry { attempts: Int }`), matched with `Name(a, b)`, `Name { field, .. }` and
  nested patterns. `match mv x` takes the parts; matching a place or a reference lends them
  (`ref` or `mut`), and an enum holding qubits must be consumed like any other (N003).
- `Option<T>` and `Result<T, E>`, built in until generics, with `Some`, `None`, `Ok` and
  `Err` written alone. `Some(x)` takes its type from `x`; the others from where they are
  used (`let x: Option<Int> = None`). `Option<Qubit>` is linear, so a `Some` qubit must be
  consumed.
- `match`, with variant, literal (`0`, `-1`, `true`, `"text"`), tuple, array and catch-all
  patterns. Every case must be covered; arms are branches for ownership (N009). A `match` on
  a measurement result runs shot by shot for now.
- `measure_in(Basis::X)` measures a qubit, or every qubit of an array, in the X, Y or Z
  basis: 0 for the +1 eigenstate. `Basis` is the standard library's enum.

### Editors

- Enums and `match` in the tree-sitter grammar and the language server (hover, go to
  definition, symbols); hover on type names, `Self` included.

- Hover explains more: doc comments (`///`, now in `spec/grammar.md`), whether a function
  runs as a circuit and whether it is reversible, struct fields and methods, built-ins,
  keywords, and kets and bras.

## [0.1.0] - 2026-11-06

The first release: the language of `spec/`, apart from what it marks *Reserved*, and a
compiler that checks, runs and exports it.

### Language

- Types: `Int`, `Float`, `Bool`, `Bit`, `String`, `Qubit`, `Observable`, `Distribution<T>`,
  tuples, fixed-size arrays and structs with methods; conversions written as calls
  (`Float(n)`).
- Kets (`|01+>`), bras for measurement tests (`<+| mv q`), `ctrl` blocks with any number of
  controls, `adj` of reversible functions, and `rev fn` (inferred, or promised).
- Ownership: linear qubits, `mv`, `ref` and `mut`, lexical lifetimes, and compile-time proofs
  that array indices differ and are in range. Errors N001–N022, each with a test.
- `unsafe` for claims the compiler can't check (`release`), and `assert` for checked facts.
- Execution: every call runs once; `sample n { … }` repeats a block and counts its results;
  `<ψ| H |ψ> within ε` is an expectation value with a chosen precision.
- The circuit rule decides which runs are circuits, including `if` and `while` on measurement
  results and helpers given measurement results; `circuit fn` promises it (N021).
- `print`, `eprint` and string interpolation with format specs (`{x:.3}`).
- Destructuring tuples and arrays (`let [a, b] = mv pair`).

### Compiler

- `noev check`, `noev run` (on the ketrs simulator, `--seed`, `--circuits`),
  `noev build --emit=plan|ir` and `noev export` (OpenQASM 3, without running anything).
- Circuits are recorded once and sampled for all shots, with classical control compiled to
  ketrs `IfElse`, `While` and `Assign`; other runs go shot by shot, and the compiler says why.
- Expectation values are exact on ketrs, and estimated by measuring grouped terms on backends
  that can't see the state.
- A backend interface at circuit and gate level, with ketrs as the first backend: an
  optional dependency (the `ketrs` feature, on by default).
- Diagnostics with source excerpts, labels and help, or one line each
  (`--message-format=short`); runtime errors point at the failing expression.
- Conformance suites `tests/ui` (errors) and `tests/run` (output), and benchmarks for every
  compiler phase.

### Editors

- `noev lsp`: a language server with diagnostics as you type, hover (types and signatures),
  go to definition and document symbols.
- A tree-sitter grammar (`editors/tree-sitter-noev`) with highlight, indent and fold queries,
  and Neovim runtime files (`editors/nvim`).

[0.1.0]: https://github.com/Aethar01/noev/releases/tag/v0.1.0
