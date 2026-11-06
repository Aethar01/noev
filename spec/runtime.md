# Runtime and backends

How a Noev program is turned into something that runs, and what it runs on. A compiled
program is native code linked to the **Noev runtime**. The runtime sends quantum operations
to a **quantum backend**: a simulator such as ketrs, or a quantum computer's control stack.
The program does not depend on which backend it runs on.

Status: design, except the backend interface (§3), which v0.1's interpreter (`noev run`)
already uses, with ketrs as its backend (§6).

```
program.nv ─▶ front end ─▶ Noev IR ─┬─▶ code generator ─▶ native object ─┐
                                    │   (Cranelift, LLVM, …)              ├─▶ binary
                                    │                    Noev runtime ────┘     │
                                    ├─▶ interpreter (noev run) ──▶ Noev runtime │
                                    └─▶ QIR, OpenQASM 3 (export)                ▼
                                                                    quantum backend
                                                                    (ketrs, hardware, …)
```

## 1. Two interfaces

There are two boundaries, and each has several implementations behind it:

| Boundary          | Between                      | Implementations                         |
| ----------------- | ---------------------------- | --------------------------------------- |
| Code generation   | Noev IR and native code      | Cranelift, LLVM; the interpreter        |
| Quantum backend   | the runtime and quantum hardware | ketrs; hardware providers; future machines |

Neither boundary leaks into the language: a program means the same thing, and with the same
seed gives the same results on a simulator, whichever code generator built it.

## 2. Code generators

Code generators lower Noev IR, after all checks, to native code. The IR is the boundary: a
code generator sees only IR, the runtime's function signatures (§4) and the target.

- **Cranelift**: fast compilation, pure Rust. The default for debug builds and `noev run`
  once it compiles rather than interprets.
- **LLVM**: slower to compile, better optimised code. The default for release builds.
- Others can be added (or replace these) without changing the front end, the IR or the
  runtime.

Classical code becomes ordinary machine code. Quantum operations become calls to the
runtime (§4). A code generator may inline runtime calls it knows, but must keep their
meaning.

## 3. Quantum backends: two levels

A backend accepts work at one or both of two levels, and says which:

- **Circuit level.** The backend receives a whole circuit (gates, measurements, and
  classical control on measurement results) and a number of shots, and returns the results
  of every shot. This is how today's quantum computers are used: a job is submitted and
  runs on the machine without stopping to ask the host anything.
- **Gate level.** The backend receives operations one at a time, as the program reaches them.
  Nothing is measured unless the program measures; when it does, the backend returns that
  result before the program sends anything else, so the program can use it immediately. Simulators work this way. So could a future quantum
  computer whose control system runs the program's classical code next to the qubits, in
  real time.

How each repeated run executes follows from [execution.md](execution.md) §4 and the
backend's levels:

| The run is…                               | Circuit-level backend   | Gate-level backend |
| ----------------------------------------- | ----------------------- | ------------------ |
| a circuit                                 | one job for all shots   | shot by shot, or as a circuit if the backend also accepts circuits |
| not a circuit                             | rejected (§5)           | shot by shot       |

A call that is not repeated (a quantum function called once from `main`) is a run with one
shot, so the same table applies.

A backend that accepts both levels, as ketrs does, takes circuits where it can, since that is
faster.

### What only simulators can do

Some things are possible only when the backend can see the state, and a backend says
whether it can do each:

- **Check `release`**: confirm that a released qubit is in \|0⟩ ([ownership.md](ownership.md)
  §11). Other backends trust the claim.
- **Exact expectation values**: compute `<ψ| H |ψ>` from the state. Other backends will
  estimate it by measuring in each term's basis (not yet implemented).
- **Sample a state without measuring it**: when a run returns qubits and measures nothing,
  simulate it once and draw every shot from the final state.

### Circuits are built while the program runs

A circuit's gates, angles and size depend on classical values computed by the program, such
as a `sample` block's captured variables or a loop bound. So the runtime builds each
circuit when the run starts: it executes the run's code once in **recording** mode, where
quantum operations are recorded instead of performed and measurement results are placeholders
(§4). The recorded circuit is what goes to the backend, in the backend's format (ketrs
circuits, OpenQASM 3 or QIR for hardware). Building a circuit is classical work that runs at
native speed; the runtime may cache a run's circuit and rebind only its angles when the same
run is repeated with new values.

The compiler knows which runs are circuits ([execution.md](execution.md) §5), and passes that
to the runtime with each run, so the runtime never has to discover it by trying.

## 4. The runtime interface

Compiled code calls the runtime through a C ABI, so code from any code generator can link
to it, and a runtime or backend can be written in any language.

Its shape follows QIR's: qubits and measurement results are opaque handles, and gates are
functions. The groups of functions:

- **Program**: start up (parse the backend's options, seed the random numbers), shut down
  (flush output, report errors).
- **Qubits**: allocate, release (with the simulator's check, [ownership.md](ownership.md)
  §11), reset.
- **Gates**: one function per core gate ([core.md](core.md)), each taking its target qubits,
  angles, and the qubits controlling it (from `ctrl`).
- **Measurement**: measure a qubit, giving a *result handle*; read a result as a `Bit`. In
  recording mode a handle stands for a measurement that has not happened. Using it in a
  condition records classical control; reading its value outside the circuit rule's
  allowed uses is an error the compiler has already ruled out for runs it marked as circuits.
- **Runs**: run a function `n` times (`sample`), given a pointer to the run's code, its
  captured values, and whether it is a circuit; returns a `Distribution`. Estimate an
  expectation value of a preparation.
- **Output and failure**: `print`, `eprint`, `assert` failures and runtime errors.

The runtime implements runs on top of the backend: it records circuits for circuit-level
backends, and calls the run's code once per shot for gate-level ones.

### QIR

QIR is LLVM IR that calls a standard set of quantum functions (`__quantum__qis__h__body`,
`__quantum__rt__result_record_output`, …). Noev uses it in two ways:

- **Export**: `noev build --emit=qir` writes a run's circuit, or a whole program, as QIR, for
  tools and hardware that accept it. Circuits without classical control fit QIR's base
  profile; circuits with it need the adaptive profile.
- **Linking**: the LLVM code generator can emit calls to QIR's functions instead of Noev's,
  so a compiled program links against any QIR runtime. Noev's runtime interface keeps the
  same shapes so that this is a renaming, not a translation.

## 5. Targets

A **target** is a code generator's machine (`x86_64-linux`, …) plus a quantum backend. The
compiler checks a program against its target's quantum backend and rejects, at compile
time, what the backend cannot run: a run that is not a circuit on a circuit-level-only
backend, more qubits than the backend has, or a gate it does not support and cannot be
decomposed into ones it does. The error points at the line that makes the run need gate-level
control, as `--emit=plan` does.

The backend is chosen when the program is built (`noev build --backend=ketrs`), and a
program can be built for a dynamic backend, loaded when it starts, so one binary can run on
a simulator or on hardware.

## 6. The interpreter

`noev run` interprets the IR instead of compiling it. It drives the same runtime and
backends through the same interface, so a program behaves the same interpreted or compiled.
The interpreter is the reference implementation: compiled programs are tested against it.

In v0.1, the interface is a Rust trait in `noev-runtime` (`Backend`, with a `Session` for gate
level), and ketrs is the only backend. Quantum code that runs outside a repeated run (in
`main` directly) still needs gate level; making it a one-shot circuit for circuit-level
backends comes with the C ABI.

## 7. `noev build` outputs

| `--emit=` | Output                                                                    |
| --------- | ------------------------------------------------------------------------- |
| `binary`  | An executable linked to the runtime and a backend (the default, later)     |
| `object`  | A native object file, to link yourself                                    |
| `qir`     | QIR                                                                       |
| `qasm3`   | OpenQASM 3, for programs or runs that are circuits                        |
| `ir`      | Noev IR, for inspecting the compiler                                      |
| `plan`    | How each repeated run executes ([execution.md](execution.md) §5)          |

## 8. Open questions

- **Backend options**: how a program or its user chooses a hardware backend, its device and
  its credentials, without baking them into the binary.
- **Result output**: how a `Distribution` from a hardware job reports its metadata (device,
  calibration, time), if the language exposes it at all.
- **Dynamic backends**: a plug-in ABI with versioning, so backends can be built separately
  from Noev.
- **Real-time limits**: a gate-level quantum computer will bound how long classical code may
  run between operations; the compiler would need to check those bounds for a target.
