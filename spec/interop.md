# Interop

Status: **Partially implemented.** Host `extern fn` declarations, boundary type and
reversibility checks (§1, §4, §5), and the safe Rust embedding API with re-entrant callbacks
(§2) are implemented in the `noev` crate (`noev::Program`, `noev::Runtime`,
`noev::HostContext`). Python bindings (§2) and C interop (§3) remain reserved; see
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
values containing qubits, and no references, except that C functions take arrays and structs
by reference (§3). This is checked where the declaration is written.

| Noev                     | Rust                         | C                    | Python          |
| ------------------------ | ---------------------------- | -------------------- | --------------- |
| `Int`                    | `i64`, checked               | `int64_t`, checked   | `int`           |
| `Float`                  | `f64`                        | `double`             | `float`         |
| `i8`…`i64`, `u8`…`u64`   | the same                     | `int8_t`…`uint64_t`  | `int`           |
| `f32`                    | `f32`                        | `float`              | `float`         |
| `Bool`                   | `bool`                       | `bool`               | `bool`          |
| `Bit`                    | `noev::Bit`                  | `uint8_t` (0 or 1)   | `int` (0 or 1)  |
| `()`                     | `()`                         | `void`               | `None`          |
| `(A, B)`                 | tuples                       | not supported        | tuples          |
| `[T; N]`                 | `[T; N]`                     | `T*`, by reference only (§3) | lists   |
| classical structs        | generated, or `noev::Value`  | `extern "C" struct` only (§3) | objects |
| `extern "C" fn(A) -> B`  | `extern "C" fn(A) -> B`      | function pointer     | not supported   |
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
// Rust host (`noev` crate).
let program = noev::Program::compile_file("vqe.nv")?;
let mut runtime = noev::Runtime::new(&program)
    .host_fn("minimize_step", |energy: f64, theta: f64| optimiser.step(energy, theta));
let energy: f64 = runtime.call("vqe", ())?;
```

To call back into Noev from inside a host function, register it with
`host_fn_with_context`: its first parameter is `&mut noev::HostContext`, which exposes the
same checked `call` method.

Python bindings expose the same API.

A host function may call back into the program while it runs, through the same `call`: a
Python optimiser can evaluate `energy(θ)` from inside the host function Noev called. The
callback runs as any other call does, creating and consuming its own qubits, and a runtime
error in it is reported to the host function as an error result.

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

These are Rust's rules for C.

**Arrays** cross only by reference. A `ref [T; N]` parameter is a `const T*` to `N` elements,
and a `mut [T; N]` parameter a `T*`, for any `T` C can take. The lend lasts for the call:
that the C function does not keep the pointer is part of what `unsafe` claims. C cannot
return an array, so results come back through a `mut` parameter:

```noev
extern "C" {
    fn normalize3(v: mut [Float; 3])
}
```

**Structs** cross only when declared `extern "C" struct`, which gives them C's layout: fields
in the order written, aligned as C aligns them. Their fields must be types C can take:
numbers, `Bool`, `Bit`, arrays of these, and other `extern "C" struct`s. They are passed by
value, or by reference as arrays are. Other structs have no defined layout, so the compiler
can reorder and split them, and they cannot cross. Neither can tuples.

```noev
extern "C" struct Point { x: Float, y: Float }
```

**Callbacks.** A function defined as `extern "C" fn` uses C's calling convention, and its name
is a value of type `extern "C" fn(A, …) -> R` that can be passed to C, as an optimiser takes
its cost function. Its parameters and result follow the rules above. It captures nothing:
it is a function, not a closure. It may run quantum code, creating and consuming its own
qubits, like any call. A runtime error inside it ends the program, since it cannot unwind
through C's stack frames. Calling a function pointer that came from C needs `unsafe`.

```noev
extern "C" {
    fn minimize(cost: extern "C" fn(Float) -> Float, start: Float) -> Float
}

extern "C" fn energy(theta: Float) -> Float {
    <ansatz(theta)| h |ansatz(theta)>
}

fn vqe() -> Float {
    unsafe { minimize(energy, 0.1) }
}
```

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


That rule settles where foreign calls can run on backends that take circuits. A circuit is
built by running its classical code once on the host ([runtime.md](runtime.md) §3), so a
foreign call that does not use a measurement result runs then, before anything is sent to the
backend. One that does makes the run not a circuit, which a backend that takes only circuits
rejects when the program is built for it, at the line that makes the run need gate-level
control.

A backend working gate by gate may run the program's classical code next to its qubits,
under time limits, where arbitrary foreign calls cannot run while qubits are alive. Its
target description says whether they can ([runtime.md](runtime.md) §5), and the compiler
rejects a foreign call made while qubits are alive on a target that says not. The program
itself never asks what kind of backend it runs on.

## 6. Grammar

```ebnf
item        = … | extern_def ;
extern_def  = "extern" fn_sig                                   (* host function *)
            | "extern" STRING "{" { fn_sig sep } "}"            (* "C" imports *)
            | "extern" STRING struct_def                        (* C layout *)
            | "extern" STRING fn_def ;                          (* callable from C *)
type        = … | "extern" STRING "fn" "(" [ type { "," type } [ "," ] ] ")" [ "->" type ] ;
```

`extern` becomes a reserved keyword. `STRING` is a string literal ([grammar.md](grammar.md)),
without interpolation; only `"C"` is accepted.

## 7. Open questions

- **Rust without going through C**: a generated two-sided bridge (like the `cxx` crate), so
  Rust libraries can be imported with checked types.
- **Closures across the C boundary**: callbacks that capture values, through C's usual
  `void*` user data.
