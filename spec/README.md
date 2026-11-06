# Noev Language Specification

Version 0.1, draft. These documents define the Noev language. The Rust compiler in
`crates/` is one implementation of them. When the compiler and the spec disagree, the
compiler is wrong, or the spec is changed on purpose.

| Document                       | Covers                                                    |
| ------------------------------ | --------------------------------------------------------- |
| [grammar.md](grammar.md)       | Lexical structure and syntax                              |
| [types.md](types.md)           | Types, linearity, literals, conversions                   |
| [ownership.md](ownership.md)   | Moves, borrows, consumption, reversible contexts, errors  |
| [core.md](core.md)             | Intrinsics: built-in gates, measurement, observables, maths |
| [std.md](std.md)               | The standard library, with a reference definition in Noev for each item |
| [execution.md](execution.md)   | Runs, shots, sampling, and when a run is a circuit         |
| [modules.md](modules.md)       | Modules, imports, visibility and packages (Reserved)      |
| [interop.md](interop.md)       | Calling Noev from other languages, and foreign functions (Reserved) |

The conformance suite in `tests/` is part of the spec. Each rule in
[ownership.md](ownership.md) has an error code, and each code has at least one compile-fail
program in `tests/ui/`.

## Design principles

1. **Classical and quantum code are both first-class.** Quantum syntax is only added where
   the physics has no classical equivalent. Classical code reads like Rust and never needs
   to use or avoid quantum features.
2. **Hide complexity, not physics.** The compiler manages qubit placement and lifetimes, but
   never silently does anything with physical consequences, such as measuring or discarding
   a qubit.
3. **Start strict, relax later.** When in doubt, reject. Accepting a program the compiler
   used to reject never breaks existing code; the reverse does.
4. **Every ownership transfer is visible.** A variable can only stop being usable at an
   explicit `mv`.
5. **Shortcuts stand for written-out code.** Where something can be left out (`mut` on a
   qubit argument, `ref`, `rev`), the compiler fills it in exactly as it would be written,
   and the meaning is defined on the written-out form. A shortcut is only allowed where one
   reading is possible and the reader loses nothing important. Tools can show the filled-in
   form (editor hints, `noev expand`).
6. **Own syntax, Rust's ownership model.** Noev borrows Rust's semantics for ownership and
   references, but its syntax differs wherever there is a reason to, so it is not mistaken
   for Rust.

## Status markers

- **v0.1**: implemented in the first compiler.
- **Reserved**: syntax is specified and parsed, but the compiler rejects it with "not yet
  supported".
- **Open**: undecided; listed in the "Open questions" section of the relevant document.
