# Run tests

End-to-end programs for the conformance suite: each must run with `noev run` and print
exactly what its `.out` file holds. Like `tests/ui`, they depend only on the command line.

## Layout

- `NAME.nv`: a program with a `fn main()`.
- `NAME.out`: its expected standard output, byte for byte.
- A first line `// fails: TEXT` instead means the run must fail (non-zero exit status) with
  `TEXT` somewhere in standard error; standard output must still match `NAME.out`.

## Determinism

Every program's output is fixed by the language, whatever the backend or random seed: its
measurements have certain outcomes, its distributions have a single outcome, and its
expectation values are of eigenstates, which are exact even when estimated. Anything
statistical belongs in an implementation's own tests.

## Running

`cargo test -p noev --test run` runs every file through:

```
noev run FILE
```
