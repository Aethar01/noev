# Testing

Noev has a built-in test system: tests are written as `test fn` functions in `.nv` files and
run with `noev test`.

## 1. Writing tests

`test` is a contextual keyword directly before `fn` (or `test(fail)` / `test(fail = "…")`
before `fn`). Everywhere else `test` is an ordinary identifier.

```noev
test fn bell_correlation() {
    let counts = sample 100 {
        let mut a = |0>
        let mut b = |0>
        h(mut a)
        ctrl a { x(mut b) }
        (mv a.measure(), mv b.measure())
    }
    assert counts.shots() == 100
    assert counts.count((0, 1)) == 0
    assert counts.count((1, 0)) == 0
}

test(fail) fn release_non_zero() {
    let mut q = |0>
    x(mut q)
    unsafe { mv q.release() }
}

test(fail = "out of range") fn bad_index() {
    assert false, "index out of range"
}
```

### Rules

- A `test fn` must be a top-level item (not inside a `struct`, `enum`, `trait`, `impl`, or
  `extern` block).
- A `test fn` must take **zero parameters**, have **no generic parameters**, and return
  **`()`**.
- A `test fn` cannot be `rev` or `extern`.
- `test fn` bodies are type-, ownership-, and target-checked by `noev check`, `noev build`,
  and `noev test`. They are only executed by `noev test`.

### Expected failures

- `test(fail) fn name()` passes if and only if running `name` terminates with a runtime error
  (such as a failed `assert`, integer overflow, division by zero, or a failed `release` check).
- `test(fail = "substring") fn name()` additionally requires the runtime error message to
  contain `substring`.

### Assertions with messages

`assert cond` checks a `Bool` condition at runtime and fails with `"assertion failed"` if
`cond` is `false`. An optional `String` expression after a comma supplies the failure message
(and is evaluated only when `cond` is `false`):

```noev
assert actual == expected, "expected {expected}, got {actual}"
```

## 2. Discovery

`noev test [PATH]` discovers tests according to `PATH` (default `.`):

- **A single `.nv` file** (`noev test foo.nv`): runs every `test fn` declared in `foo.nv`.
- **A package directory** (contains `noev.toml`):
  1. **Unit tests**: every `test fn` in the package's `src/` modules (each sees its module's
     private items).
  2. **Integration tests**: every `.nv` file directly in `tests/` (compiled as a separate test
     root with the package's library available as a dependency under its package name).
- **A directory without `noev.toml`** (such as a conformance suite): runs every `test fn` in
  every `.nv` file directly in that directory, sorted by filename.

Test names are qualified by module path within a package (`ansatz::ry_preserves_norm`), or by
file stem when running a directory of `.nv` files (`gates::toffoli`).

## 3. Running tests

```
noev test [PATH] [FILTER] [OPTIONS]
```

| Option | Effect |
| --- | --- |
| `FILTER` | Run only tests whose full name contains `FILTER` |
| `--list` | Print matching test names without running them |
| `--seed N` | Base PRNG seed for reproducible quantum test runs (each test derives a deterministic seed from `N` and its index) |
| `--backend NAME[:DEVICE]` | Backend to run tests on (`ketrs` by default, or `[target] backend` from `noev.toml`, or a plug-in/cached target) |
| `--compile` | Compile tests to a native test runner binary instead of running in the interpreter |
| `--codegen cranelift\|llvm` | Compile and run tests natively with the chosen code generator (implies `--compile`) |
| `-O, --opt-level <0..3>` | Overall optimization level (`0` = none, `1` = light, `2` = default, `3` = aggressive) for native codegen and circuit optimization |
| `--circuit-opt <0..3>` | Override quantum circuit optimization level independently of `-O` |
| `--cranelift-opt <none\|speed\|speed-and-size>` | Override Cranelift codegen optimization setting independently of `-O` |
| `--llvm-opt <0\|1\|2\|3\|s\|z>` | Override LLVM codegen optimization level independently of `-O` |
| `--nocapture` | Print each test's standard output as it runs instead of hiding output for passing tests |

Each test runs in an isolated runtime state with its own fresh quantum state and PRNG seed.
On failure, `noev test` prints any captured output, the failure diagnostic, and the `--seed`
needed to reproduce the run, and exits with status `1`.
