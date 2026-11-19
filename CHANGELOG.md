# Changelog

Notable changes to Noev, the language and this compiler. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/), with breaking changes allowed in minor versions
before 1.0.

## [Unreleased]

### Spec

- 0.5 is planned in `docs/PLAN.md`: modules, generics, native code, interop and backends.
- Quantum integers are dropped from the plan: a library can define one on generics, size
  parameters and `rev` methods. Operator traits stay under "not yet" in `spec/types.md` §6.
- Modules follow Rust: glob imports, `pub(pkg)`, `pub(super)` and `pub(in path)`, and a small
  prelude, so `State`, `amplitudes`, `fft`, `ifft`, `P` and `PX` will need a `use`.
- C interop follows Rust: arrays by reference, `extern "C" struct` for C layout, and
  `extern "C" fn` callbacks. Host functions can call back into the program.
- Bounds are inferred from bodies within a package, `Copy` included: a body that copies or
  drops a `T` needs `T: Copy`, one that calls `x.measure()` needs `T: Measure`, and needs
  follow calls. A call giving a type without what a function needs is an error at the call,
  pointing at the line that needs it. A bound written is a promise, as are a library's `pub`
  functions' bounds, so copying a `T: Measure` is still N001.
- A type can implement a generic trait several times with different arguments; a call picks
  the `impl` by the arguments inferred from it, or by the type's only `impl` or bound when
  they leave one open, and is an error otherwise. A generic trait's associated types depend
  on its arguments (`Scale<Int>` and `Scale<Float>` can each give a type its own `Out`).
- Operator traits: `+`, `-`, `*` and unary `-` on a program's own types call `std::ops`'s
  `Add`, `Sub`, `Mul` and `Neg`, picked by the right side's type, so `v * 2` and `v * w` can
  differ. Sides holding qubits are moved with `mv`, as arguments are.
- Being a circuit is decided across modules and packages by the compiler, as within one;
  only reversibility needs a declaration (`rev fn`) to cross a module.
- Backends are libraries implementing `std::backend::Backend`, in Noev, Rust or C. Every
  backend has a target description with its capabilities, so a program cannot tell a
  simulator from a quantum computer; what used to be "simulators only" is now "backends that
  can read the state".

### Compiler

- Spans are offsets into a source map in which each file has its own range, so diagnostics
  find their file and can point into several files (`spec/modules.md` §8). The lexer, parser
  and lowering take a file's starting offset (`lex_at`, `parse_at`).
- `pub`, `pub(pkg)`, `pub(super)`, `pub(self)` and `pub(in path)` are parsed on items, fields
  and methods, and `use` trees with braces, `self`, `*` and `as`, flattened into one import
  per name. `pub` on a variant or an `impl` block is an error, as in Rust.

- Packages: `noev check`, `build`, `run` and `export` take a `.nv` file or a package
  directory (the current one by default). A package has a `noev.toml` and `src/main.nv` or
  `src/lib.nv`; every `.nv` file under `src/` is a module, and dependencies are other package
  directories, given by `path`.
- Names are resolved per module, as in Rust: a module sees its own items, its imports, then
  its glob imports, then its dependencies, then the prelude. `use` (with globs and
  re-exports), paths through `pkg`, `super`, `self`, modules and packages, and the
  visibility of items are checked. Imports resolve in any order, and a chain of re-exports
  in one pass.
- Fields and methods are private to their module unless marked: other modules cannot read
  a private field, create a struct with one, or call a private method.
- A function without `rev` is reversible only inside its own package; from another package a
  call to it in reversible code or under `adj` is N013 or N014, with a note saying why. A
  library's `pub` function that acts on qubits and is reversible only by inference gets a
  warning.
- **Breaking:** the prelude no longer has `State` (now `std::state`), `amplitudes`
  (`std::debug`), `fft` and `ifft` (`std::fourier`), or `P` and `PX` (`std::observables`).
  Using one without its `use` is an error whose help gives the line to add.
- Functions outside a package's root module are named by their path in the IR (`ansatz::ry_layer`,
  `qlib::flip`), and `fn main` is only the one in the root module.

- Generics are parsed: type and size parameters (`<T: Copy, const N: Int>`) on
  functions, structs, enums and traits, `where` clauses, generic `impl`s with bounds such as
  `Measure<Result = Bit>`, size parameters as array lengths, sizes as type arguments
  (`Register<8>`) and `swap::<Int>`.
- Generic functions, structs and enums with type parameters: a plain `T` takes classical
  values and qubits alike and is moved and consumed like a qubit (N001 and N003 say so and
  suggest `T: Copy`); `T: Copy` lets a body copy and drop. Type arguments are inferred from
  arguments, fields and expected types, or given with `::<…>`; each generic function is
  compiled once per list of type arguments (`unwrap_or<Int>` in the IR). `Option` and
  `Result` are instances of generic definitions like any other.
- Traits and `impl`s: default functions, generic `impl`s (`impl<T: Shape> Shape for
  Pair<T>`), calls on concrete types and through bounds (`T: Shape`), and `rev` trait
  functions, which every `impl` must keep and generic code can invert with `adj`. An `impl`
  must provide every function without a default, with the trait's signature, be in the
  package of the trait or the type, and not give a trait twice for one type. A call through a
  trait function not declared `rev` through a bound is irreversible, and runs shot by shot;
  on a known type the call reaches the `impl`'s function, whose reversibility is inferred.
  `Trait::f(x)` names the trait, and is needed when two traits of a type share a name.
- Reversibility, preparations and printing are inferred in time linear in the program:
  each function is looked at again only when one it calls changes, instead of every
  function on every pass. A chain of 2000 functions declared last to first, the first
  irreversible, took 95 ms to check for ownership and now takes 2.5 ms.
- A generic function calling itself infers its own type parameters (`f(s, n - 1)` inside
  `f<T>`), which used to be "cannot tell what `T` is".
- Associated types: `type Result: Copy` in a trait, `type Result = Bit` in each `impl`,
  `T::Result` and `Self::Result` in signatures, and bounds that fix one
  (`T: Readout<Result = Bit>`), checked at each call.
- `Measure` is a trait in the prelude: a struct measures to a classical result by an
  `impl` (`Experiment` to `ExperimentResult`), and `x.measure()`, `measure(x)` and generic
  code with `T: Measure` measure it. Qubits implement it with the built-in measurement,
  and tuples and arrays of measurable types by measuring each element.
- Size parameters: `const N: Int` on functions, structs and `impl`s, `[T; N]`, `N` as an
  `Int` value and `[|0>; N]`. Sizes are inferred from arguments (`[Qubit; 4]` for
  `[Qubit; N]`) or given (`fresh::<4>()`, `Reg<4>`), and each size gets its own copy.
  Indices into a qubit array of length `N` are proven against `N` (N017, N018): `for i in
  0..N - 1` proves `r[i + 1]` in range, and `N - 1 - i` is at least 0 inside `0..N`.
- Type and size arguments that a call's arguments leave unknown are inferred from the type
  expected of the call: `let r: [Qubit; 2] = fresh()`, `let x: Option<Int> = none()`.
- An `impl` for a generic type (`impl<const N: Int> Measure for Reg<N>`) finds its own
  associated types in its signatures.
- Generic traits: `trait Oracle<const N: Int>`, `trait Scale<T>`, bounds such as
  `T: Oracle<N>`, and one type implementing a trait with several arguments (`impl Oracle<2>
  for Flip` and `impl Oracle<3> for Flip`), the call picking by its arguments, or the only
  `impl` or bound when they leave it open. Defaults see the trait's parameters. The IR names
  each `impl`'s functions with the arguments: `<Flip as Oracle<2>>::apply`. Associated
  types in generic traits are not supported yet.
- Inferred bounds: a generic function whose parameter has no bound written, and that other
  packages cannot call, infers `T: Copy` when its body only checks with `T` copyable (the
  copy or drop that needs it is shown at a call that gives a qubit), and the trait of a
  method called on a `T` (`x.area()`) or of a bound it is passed to; needs follow calls,
  through recursion too, and are checked at each call once every body is. A method two
  traits share needs the bound written.

- ketrs 0.2.0. Noev uses none of its new features (symmetric MPS, batched MPS
  expectations), and nothing changes for programs.

### Tools

- The tree-sitter grammar and Neovim highlights know visibilities, `use` and generics; an
  `impl` of a trait named by one identifier parses again.
- The language server checks a file in a package with the rest of the package, reading open
  documents from the editor; it rechecks the package's other open files when one changes, and
  go to definition and diagnostic notes can point into other files.
- `noev build --emit=plan` says a function calling a trait's function that is not `rev fn`
  is not a circuit because the `impl` may measure, instead of claiming a measurement result is
  passed to it, and finds each file's lines once, so long programs no longer take quadratic
  time (a chain of 4000 functions: 356 ms to 51 ms).

## [0.4.2] - 2026-11-08

### Spec

- The spec is marked version 0.4 and no longer calls the current compiler "v0.1": features
  that already exist are marked **Implemented**, and the rest say "not implemented yet".
  `std::io` needs only modules now, since `Result` came in 0.2.
- The help on "traits are not supported yet" and "`with`/`do` is not supported yet" no
  longer says they come after v0.1.

## [0.4.1] - 2026-11-08

### Compiler

- Two-qubit matrix gates are decomposed with the fewest CNOTs (the KAK decomposition): none
  for a product of one-qubit gates, one for a CNOT's class, two for iSWAP's, three in
  general. It used to take up to six two-qubit gates; that method remains as a fallback if
  the numbers ever fail to settle, so the result is always exact.

## [0.4.0] - 2026-11-08

### Language

- Complex numbers: `Complex` (two `Float`s, also called `c64`, like Python's `complex`) and
  `c32` (two `f32`s), written with imaginary literals (`1.0 + 2.0i`, `3i`). `+ - * /`, `==`,
  `re()`, `im()`, `abs()`, `arg()`, `conj()`, `exp()`, `sqrt()` and `Complex::polar(r, theta)`;
  `Complex(x)` and `c32(z)` convert, and nothing converts implicitly. They print as
  `1.0+2.0i`.
- Amplitudes `<φ|ψ>` and transition values `<φ| H |ψ>` (different preparations on the two
  sides), as `Complex` numbers: `<0|+>`, `<bell()|00>`, `<0| X(0) |1>`. Exact on a
  simulator, estimated with Hadamard tests elsewhere, `within ε` for each part. Both sides
  must be reversible preparations (N023).
- Gates from matrices: `q.unitary(m)` with `m: [[Complex; 2]; 2]`, and `r.unitary(m)` on a
  `[Qubit; 2]` with a 4×4 matrix over \|00⟩, \|01⟩, \|10⟩, \|11⟩. Checked to be unitary when
  they run; reversible, so `adj` and `ctrl` work. A one-qubit matrix exports as
  `U(θ, φ, λ)` with its global phase under control; a two-qubit one is decomposed, for
  circuits and under `ctrl`, into one-qubit matrices controlled by the other qubit.
- `amplitudes(r)`, for debugging on a simulator: a register's state as a `[Complex; 2^N]`,
  over \|00⟩, \|01⟩, … with the first non-zero amplitude real and positive; an error if the
  register is entangled with other qubits. A `sample` that reads amplitudes runs shot by shot.
- `fft(x)` and `ifft(x)` on `[Complex; N]` and `[c32; N]`, as NumPy computes them (radix-2
  for powers of two, any length otherwise).
- Arrays and tuples compare with `==` and `!=`, element by element, when their elements do:
  `m == [0, 1]` for measured bits, which a circuit records as one condition.
- A method call may follow an expectation value or amplitude directly:
  `<ψ| H |ψ>.round()`, `<φ|ψ>.abs()`.

### Compiler

- ketrs 0.1.2: circuits that measure mid-circuit follow each measurement history once
  instead of every shot, so feedback and repeat-until-success circuits sample far faster.
- Indexing or taking a field of a value that is not a variable, such as a call's result
  (`g()[1]`, `p().y`), no longer overflows the compiler's stack.

### Editors

- Hover on `Complex`, `c32` and their methods; imaginary literals, amplitudes and transition
  values in the tree-sitter grammar.

## [0.3.0] - 2026-11-07

### Language

- Kets and bras for the Y basis: `(+i)` and `(-i)`, a label in parentheses being one qubit
  (`|0(+i)>` is two qubits, `<(-i)| mv q` tests for \|−i⟩).
- Type aliases: `type Byte = [Bit; 8]` is another name for the same type, including its
  variants, associated functions and literals for a struct or enum.
- `Int` has any size and never overflows (`1 << 100`, `factorial(30)`, literals of any
  length); values that fit in 64 bits stay machine integers. `/` truncates and `%` takes the
  dividend's sign; bitwise operators act as on endless two's complement. Converting a
  `Float` to an `Int` is exact (it used to saturate), and NaN or an infinity is an error.
- Sized numbers: `i8`…`i64`, `u8`…`u64` and `f32`, each its own type; `f64` is another name
  for `Float`, which stays a 64-bit float like Python's `float`. Literals take the type
  expected (`let x: u8 = 200`, `x + 1`), and one that does not fit is an error. Overflow is
  a runtime error; shifts drop the bits shifted out. `i64(x)` converts when every value
  fits, `u8::try_from(n)` gives a `Result` otherwise, and `u8(bits)` and `x.bits()` read a
  `[Bit; 8]` as a number and back, most significant bit first.
- A bra can hold a preparation: `<bell()| mv pair` is `true` when `pair` was in the state
  `bell()` makes, undoing `bell()` on it and testing for \|0⟩. The preparation must be
  reversible apart from creating its qubits (N023).
- Computed labels in kets and bras: `|0{b}>`, `|{bits}>`, `<{bits}| mv r`, where `{…}` is a
  `Bit`, a `State` (the standard library's six basis states) or an array of them. A label
  computed from a measurement result prepares its qubit conditionally, inside the circuit.

### Compiler

- `adj` of a function that itself calls `adj` inverts in the right order; the inner inverse
  used to be applied at once.
- Runtime errors in compound assignments (`d /= 0`) point at the statement.
- Type checking is about 10% faster: the built-in types skip the type table's hash map.

### Editors

- Hover on built-in types (`Int`, `u8`, …) and on `try_from`.
- Hover on a type in a parameter's annotation shows the type, not the parameter; aliases
  have hover, go to definition and symbols.

## [0.2.0] - 2026-11-07

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
  patterns. Every case must be covered, looking inside tuples, arrays and variants' data
  (`(0, 0)`, `(0, 1)`, `(1, _)` cover a pair of bits), and an error names a value no arm
  takes, such as `(1, 1)` or `Some(1)`. Arms are branches for ownership (N009).
- `measure_in(Basis::X)` measures a qubit, or every qubit of an array, in the X, Y or Z
  basis: 0 for the +1 eigenstate. `Basis` is the standard library's enum.

### Compiler

- A `match` on measurement results is classical control in the circuit, as an `if` is: one
  `if` per test its patterns make, and none for a test an enclosing one already decided.
  Teleportation corrected by a `match` is one circuit with four branches.

### Editors

- Enums and `match` in the tree-sitter grammar and the language server (hover, go to
  definition, symbols); hover on type names, `Self` included.
- Hover and go to definition on fields everywhere: where they are declared, in struct
  literals (`Retry { attempts: 2 }`) and in patterns; on variants where they are declared
  and where they are called with their data.
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

[0.4.2]: https://github.com/Aethar01/noev/releases/tag/v0.4.2
[0.4.1]: https://github.com/Aethar01/noev/releases/tag/v0.4.1
[0.4.0]: https://github.com/Aethar01/noev/releases/tag/v0.4.0
[0.3.0]: https://github.com/Aethar01/noev/releases/tag/v0.3.0
[0.2.0]: https://github.com/Aethar01/noev/releases/tag/v0.2.0
[0.1.0]: https://github.com/Aethar01/noev/releases/tag/v0.1.0
