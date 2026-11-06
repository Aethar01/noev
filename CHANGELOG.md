# Changelog

Notable changes to Noev, the language and this compiler. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/), with breaking changes allowed in minor versions
before 1.0.

## [0.1.0] - Unreleased

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
