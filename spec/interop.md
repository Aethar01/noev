# Interop

Status: **Reserved.** Nothing here is in v0.1; the syntax and rules are fixed now so that the
type checker and runtime are designed with them in mind. The order they arrive in is in
`docs/PLAN.md` §11.

Hybrid programs lean on classical libraries Noev will not reimplement (optimisers, linear
algebra, machine learning, plotting), and many users will want to call Noev from an existing
program. Interop works in both directions:

1. **Embedding:** a host program (Rust, Python, C) loads a Noev program and calls its
   functions.
2. **Imports:** a Noev program calls functions provided by the host or by a C library.

## 1. What crosses the boundary

**Only classical values.** Foreign code knows nothing about Noev's ownership rules: a qubit
handed to it could be copied or forgotten, breaking the guarantees in
[ownership.md](ownership.md). So every parameter and result of a foreign function, and of a
Noev function called by a host, must be **copyable** ([types.md](types.md) §5): no qubits, no
values containing qubits, and no references. This is checked where the declaration is
written.

| Noev                     | Rust                         | C                    | Python          |
| ------------------------ | ---------------------------- | -------------------- | --------------- |
| `Int`                    | `i64`, checked               | `int64_t`, checked   | `int`           |
| `Float`                  | `f64`                        | `double`             | `float`         |
| `Bool`                   | `bool`                       | `bool`               | `bool`          |
| `Bit`                    | `noev::Bit`                  | `uint8_t` (0 or 1)   | `int` (0 or 1)  |
| `()`                     | `()`                         | `void`               | `None`          |
| `(A, B)`, `[T; N]`       | tuples, `[T; N]`             | not supported at first | tuples, lists |
| classical structs        | generated, or `noev::Value`  | not supported at first | objects       |
| `Observable`             | `noev::Observable`           | not supported        | object          |

"Checked": an `Int` outside the 64-bit range is an error where it crosses.

Quantum work is done by calling Noev functions that create and consume their own qubits,
such as a function returning an expectation value. The host never holds a qubit.

## 2. Host functions

A host function is provided by the program embedding Noev and declared in Noev with
`extern fn`:

```noev
extern fn minimize_step(energy: Float, theta: Float) -> Float

fn vqe() -> Float {
    let mut theta = 0.1
    for i in 0..50 {
        theta = minimize_step(<ansatz(theta)| h |ansatz(theta)>, theta)
    }
    <ansatz(theta)| h |ansatz(theta)>
}
```

The host registers an implementation under the same name. When the program is loaded, the
runtime checks that every `extern fn` has an implementation with matching types, and refuses
to run otherwise. Host functions are written in a memory-safe language behind a checked
interface, so calling one is **safe**: no `unsafe` is needed.

```rust
// Rust host (sketch).
let program = noev::Program::compile_file("vqe.nv")?;
let mut runtime = noev::Runtime::new(&program)
    .host_fn("minimize_step", |energy: f64, theta: f64| optimiser.step(energy, theta));
let energy: f64 = runtime.call("vqe", ())?;
```

Python bindings expose the same API.

## 3. C functions

A C function is declared in an `extern "C"` block and called inside `unsafe`, because C code
can do anything, including break memory safety:

```noev
extern "C" {
    fn erf(x: Float) -> Float
}

fn error_rate(x: Float) -> Float {
    unsafe { erf(x) }
}
```

The claim made by `unsafe` here ([ownership.md](ownership.md) §11) is that the C function
behaves as its declaration says. C functions are linked when the program is built (native
compilation) or loaded dynamically (the interpreter).

A C API (`noev.h`) also lets C and C++ programs embed Noev, with the same type mapping.

## 4. Rules for foreign calls

- Foreign calls (host functions and C functions) are not reversible, so they are not allowed
  in reversible contexts (N013).
- A preparation in an expectation value is run many times, so foreign calls inside it run
  many times too.
- Inline OpenQASM, for hardware-specific gates, will be an `unsafe` foreign construct of its
  own; its syntax is not decided.

## 5. Where foreign calls can run

A foreign call that uses a measurement result before a run's last quantum operation makes
the run not a circuit ([execution.md](execution.md) §4).


On a simulator, a foreign call can appear anywhere a classical call can. On real hardware,
code that runs **while qubits are alive** (between the gates of one circuit) runs on the
control electronics, under tight latency limits, so arbitrary foreign calls cannot run there.
Foreign calls between runs, or around whole quantum computations, are fine.

When hardware targets exist, the compiler will have to tell these apart: either reject a
foreign call made while qubits are alive, or split the program around it. Which is part of
optimising the whole hybrid program (research question 3 in the plan).

## 6. Grammar

```ebnf
item        = … | extern_def ;
extern_def  = "extern" fn_sig                                   (* host function *)
            | "extern" STRING "{" { fn_sig sep } "}" ;          (* "C" *)
```

`extern` becomes a reserved keyword. `STRING` is a string literal; Noev has none yet, so this
form waits for them.

## 7. Open questions

- **Rust without going through C**: a generated two-sided bridge (like the `cxx` crate), so
  Rust libraries can be imported with checked types.
- **Structs and arrays across the C boundary**: layout rules, or passing them only by
  pointer.
- **Callbacks**: whether foreign code can call back into Noev during a call.
- **Hardware rule**: rejecting foreign calls while qubits are alive, or splitting programs
  around them (§5).
