# Runtime and backends

How a Noev program is turned into something that runs, and what it runs on. A compiled
program is native code linked to the **Noev runtime**. The runtime sends quantum operations
to a **quantum backend**: a simulator or a quantum computer's control stack.
The program does not depend on which backend it runs on.

```
program.nv ─▶ front end ─▶ Noev IR ─┬─▶ code generator ─▶ native object ─┐
                                    │                                     ├─▶ binary
                                    │                    Noev runtime ────┘     │
                                    ├─▶ interpreter (noev run) ──▶ Noev runtime │
                                    └─▶ QIR, OpenQASM 3 (export)                ▼
                                                                    quantum backend
                                                                    (simulator, hardware, …)
```

## 1. Two interfaces

There are two boundaries, and each has several implementations behind it:

| Boundary          | Between                      | Examples                                 |
| ----------------- | ---------------------------- | ---------------------------------------- |
| Code generation   | Noev IR and native code      | Fast JIT/AOT, optimising LLVM, interpreter |
| Quantum backend   | the runtime and quantum hardware | simulators, hardware providers, future machines |

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

A backend that accepts both levels (such as a statevector simulator) takes circuits where
it can, since that is faster.

### Capabilities

Some things are possible only when the backend can see the state. Each is a **capability**
that a backend's target description declares (§5); the language never asks whether a backend
is a simulator, so a quantum computer that could see its state would declare them too:

- **Read the state**: give a register's amplitudes (`std::debug::amplitudes`,
  [std.md](std.md)). A program that reads the state is rejected for a target without this
  capability.
- **Check `release`**: confirm that a released qubit is in \|0⟩ ([ownership.md](ownership.md)
  §11). Other backends trust the claim.
- **Exact expectation values and amplitudes**: compute `<ψ| H |ψ>` and `<φ|ψ>` from the
  state. Other backends estimate them `within ε`, by measuring each group of terms in its
  basis, as one circuit when the preparation is a circuit ([types.md](types.md) §3), and
  `noev export` saves those circuits.
- **Sample a state without measuring it**: when a run returns qubits and measures nothing,
  simulate it once and draw every shot from the final state.

### Backends are libraries

A backend implements the trait `std::backend::Backend`, at one or both levels, written in
Noev, or in another language through the plug-in C ABI (§4):

```noev
trait Backend {
    fn target(ref self) -> Target                                    // §5
    fn run(mut self, circuit: ref Circuit, shots: Int) -> Result<Shots, BackendError>
    fn session(mut self) -> Result<Session, BackendError>            // gate level
}
```

A `Circuit` is a classical value that describes a circuit: its qubits, its gates with their
angles and controls, its measurements, and its classical control. It holds no qubits, so a
backend can copy it, store it or send it over a network. A backend never holds a qubit, and
code that implements a backend cannot create one: running it would need a backend of its
own. That is a compile error.

A backend's package can offer anything else of its own: jobs, queues, credentials, device
calibration. A program that runs on that backend sees none of it. Its meaning, and with the
same seed its results on a simulator, do not depend on the backend.

### Circuits are built while the program runs

A circuit's gates, angles and size depend on classical values computed by the program, such
as a `sample` block's captured variables or a loop bound. So the runtime builds each
circuit when the run starts: it executes the run's code once in **recording** mode, where
quantum operations are recorded instead of performed and measurement results are placeholders
(§4). The recorded circuit is what goes to the backend, in the backend's format (native
simulator circuits, OpenQASM 3 or QIR for hardware). Building a circuit is classical work that runs at
native speed; the runtime may cache a run's circuit and rebind only its angles when the same
run is repeated with new values.

The compiler knows which runs are circuits ([execution.md](execution.md) §5), and passes that
to the runtime with each run, so the runtime never has to discover it by trying.

## 4. The runtime interface

Compiled code and the interpreter both call the runtime through a C ABI (`extern "C"`), so
code from any code generator can link to it, and a runtime or backend can be written in any
language.

Its shape follows QIR's: an opaque runtime handle (`NoevRuntime*`), pointer-sized opaque
qubit handles (`NoevQubit`), tagged measurement result handles (`NoevResult`: bit 63 clear
for a concrete `0`/`1` bit, bit 63 set for a recorded circuit classical bit index), and one
function per core gate. Operations that can fail return `bool` (`true` on success) and store
their diagnostic on the runtime handle.

### Types

- `NoevRuntime` — opaque execution context holding the backend session or circuit recorder,
  free qubit pool, `ctrl` stack, `adj` and `with`/`do` recording frames, PRNG, output streams
  and last error.
- `NoevQubit` — opaque pointer-sized qubit handle (`usize`).
- `NoevResult` — `uint64_t` measurement handle (concrete `0`/`1`, or `0x8000_0000_0000_0000 | clbit` while recording a circuit).
- `NoevComplex` — `struct { double re; double im; }`.
- `NoevPauliFactor` — `struct { NoevQubit qubit; uint8_t pauli; }` (`0` = X, `1` = Y, `2` = Z).
- `NoevPauliTerm` — `struct { double coeff; const NoevPauliFactor* factors; size_t len; }`.

### Functions

- **Program and errors**:
  - `noev_rt_new_export(uint64_t seed) -> NoevRuntime*`, `noev_rt_free(NoevRuntime*)`, plus
    backend-specific constructors (for example `noev_rt_new_ketrs` for the `ketrs` simulator).
  - `noev_rt_has_error(const NoevRuntime*) -> bool`,
    `noev_rt_error_message(const NoevRuntime*, size_t* out_len) -> const uint8_t*`,
    `noev_rt_error_is_unknown(const NoevRuntime*) -> bool`,
    `noev_rt_clear_error(NoevRuntime*)`,
    `noev_rt_set_error(NoevRuntime*, const uint8_t* msg, size_t len)`.
  - `noev_rt_flush(NoevRuntime*) -> bool`,
    `noev_rt_next_seed(NoevRuntime*) -> uint64_t`,
    `noev_rt_measured(const NoevRuntime*) -> bool`.
- **Qubits**:
  - `noev_rt_qubit_allocate(NoevRuntime*, NoevQubit* out) -> bool`.
  - `noev_rt_qubit_release(NoevRuntime*, NoevQubit q) -> bool` (checks `|0⟩` on simulators,
    [ownership.md](ownership.md) §11).
  - `noev_rt_qubit_reset(NoevRuntime*, NoevQubit q) -> bool`.
- **Core gates** (pick up active `ctrl` qubits and record into active `adj` / `with` frames):
  - `noev_qis_x`, `noev_qis_y`, `noev_qis_z`, `noev_qis_h`, `noev_qis_s`, `noev_qis_s_adj`,
    `noev_qis_t`, `noev_qis_t_adj`: `(NoevRuntime*, NoevQubit q) -> bool`.
  - `noev_qis_rx`, `noev_qis_ry`, `noev_qis_rz`, `noev_qis_p`:
    `(NoevRuntime*, double theta, NoevQubit q) -> bool`.
  - `noev_qis_u`, `noev_qis_u_adj`:
    `(NoevRuntime*, double theta, double phi, double lam, NoevQubit q) -> bool`.
  - `noev_qis_swap(NoevRuntime*, NoevQubit q1, NoevQubit q2) -> bool`.
  - `noev_qis_unitary(NoevRuntime*, const NoevQubit* targets, size_t n_targets, const double* params, size_t n_params, bool adjoint) -> bool`.
- **Control, adjoint and `with`/`do`**:
  - `noev_rt_ctrl_push(NoevRuntime*, const NoevQubit* controls, size_t len)`,
    `noev_rt_ctrl_pop(NoevRuntime*)`,
    `noev_rt_ctrl_depth(const NoevRuntime*) -> size_t`,
    `noev_rt_ctrl_restore(NoevRuntime*, size_t depth)`.
  - `noev_rt_adj_start(NoevRuntime*)`, `noev_rt_adj_end(NoevRuntime*) -> bool`,
    `noev_rt_adj_abort(NoevRuntime*)`.
  - `noev_rt_with_start(NoevRuntime*)`, `noev_rt_with_do(NoevRuntime*)`,
    `noev_rt_with_end(NoevRuntime*) -> bool`.
- **Measurement**:
  - `noev_qis_mz(NoevRuntime*, NoevQubit q, NoevResult* out) -> bool`.
  - `noev_rt_result_is_clbit(NoevResult) -> bool`,
    `noev_rt_result_clbit_index(NoevResult) -> size_t`,
    `noev_rt_result_read(NoevRuntime*, NoevResult) -> uint8_t` (reads a concrete bit, or
    resolves against the replay outcome when re-running classical code for a recorded circuit).
- **State inspection and runs**:
  - `noev_rt_amplitudes(NoevRuntime*, const NoevQubit* qubits, size_t n_qubits, NoevComplex* out, size_t out_len) -> bool`.
  - `noev_rt_zero_amplitude(NoevRuntime*, NoevComplex* out) -> bool`.
  - `noev_rt_expectation_exact(NoevRuntime*, const NoevPauliTerm* terms, size_t n_terms, double* out) -> bool`.
  - `noev_rt_sample_run(NoevRuntime*, size_t shots, bool is_circuit, NoevRunFn body, void* env, NoevSampleCallback on_outcome, void* cb_env) -> bool`
    runs a compiled or interpreted shot callback `shots` times — recording a circuit once when
    `is_circuit` and the backend accepts circuits, or running gate by gate otherwise.
- **Output and failure**:
  - `noev_rt_write(NoevRuntime*, bool stderr, const uint8_t* bytes, size_t len) -> bool`.
  - `noev_rt_assert(NoevRuntime*, bool cond, const uint8_t* msg, size_t len) -> bool`.

The runtime implements runs on top of the backend: it records circuits for circuit-level
backends, and calls the run's code once per shot for gate-level ones.

Backends plug into the runtime through a C ABI too, in the other direction: the runtime calls
the backend's functions for the `Backend` trait (§3). A backend written in Noev is compiled
to the same functions, so built-in simulators, C plug-ins and backends written in Noev are
interchangeable.

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

A **target** is a code generator's machine (`x86_64-linux`, …) plus a quantum backend. Every
backend, simulator or quantum computer, has a **target description**:

- the levels it accepts (circuit, gate, or both);
- its number of qubits, its gates and which qubits they can join;
- its capabilities (§3);
- for gate-level backends, whether foreign calls can run while qubits are alive
  ([interop.md](interop.md) §5).

The compiler checks a program against its target's description and rejects, at compile
time, what the backend cannot run: a run that is not a circuit on a circuit-level-only
backend, more qubits than the backend has, a gate it does not support and cannot be
decomposed into ones it does, or a capability it lacks. The error points at the line that
needs it, as `--emit=plan` does for gate-level control.

The backend is chosen with `--backend=NAME[:DEVICE]`, on `noev build` and on `noev run`; a
package can name a default in its `noev.toml`:

```toml
[target]
backend = "ketrs"
```

Built-in backends come with their descriptions. Others, such as a remote device, are
fetched once and cached, so building needs no network:
`noev target fetch NAME[:DEVICE]`. Credentials are never part of a package or a binary: a
backend reads its own when the program starts, from the environment or the user's
configuration.

A program can also be built for a dynamic backend, loaded when it starts, so one binary can
run on a simulator or on hardware. Compiled binaries accept `--backend=NAME[:DEVICE]` (or
`--backend NAME[:DEVICE]`, stopping before `--`, or `NOEV_BACKEND`) and `--seed=N` to choose
the backend and RNG seed at launch. It is then checked against the loaded backend's
description when it starts, as host functions are checked when it is loaded
([interop.md](interop.md) §2), and refuses to run if it needs what the backend lacks.

## 6. The interpreter

`noev run` interprets the IR instead of compiling it. It drives the runtime through the same
C ABI (§4), which talks to backends through the `Backend` / `Session` interface, so a
program behaves the same interpreted or compiled. The interpreter is the reference semantics:
compiled programs must agree with it.

`main` is a run with one shot, like any other call (§3). On a backend that works gate by gate
it runs as written. On one that only takes circuits, a `main` that is a circuit is recorded,
run as a one-shot job, and then its classical code, including what it prints, runs for that
outcome; the repeated runs it starts are computed once, while recording. `noev export` saves
`main`'s circuit the same way. A `main` that is not a circuit needs a gate-level backend.

## 7. `noev build` outputs

| `--emit=` | Output                                                                    |
| --------- | ------------------------------------------------------------------------- |
| `binary`  | An executable linked to the runtime and a backend (the default)           |
| `object`  | A native object file, to link yourself                                    |
| `llvm-ir` | Full-program LLVM IR text (`.ll`) from the LLVM code generator            |
| `qir`     | QIR                                                                       |
| `ir`      | Noev IR, for inspecting the compiler                                      |
| `plan`    | How each repeated run executes ([execution.md](execution.md) §5)          |

`binary` (the default) and `object` are produced by a native code generator (selected with
`--codegen`). Passing `--qir-runtime` with an LLVM code generator emits calls to QIR's
runtime symbols (`__quantum__qis__*`, `__quantum__rt__*`) for linking against external QIR
runtimes. `qir` emits QIR 1.0 LLVM IR text (`.ll`) with Base Profile for circuits and
Adaptive Profile for mid-circuit measurement, reset, and classical control;
`noev export --format=qir` saves one `.ll` file per circuit.

### Saving circuits

A circuit is built by the program's classical code (§3), but building it does not need it
to run: so circuits can be saved without a simulator or a quantum computer, however large
they are.

- `noev export FILE [-o DIR]` runs only the classical code, on the **export** backend: each
  circuit is recorded instead of run and written as OpenQASM 3, `DIR/circuit-1.qasm`,
  `circuit-2.qasm`, … in the order the program makes them, each with its shot count in a
  comment, or all on standard output without `-o`. What the program prints is dropped.
  `noev run FILE --backend=export [--circuits=DIR]` is the same thing, chosen the way any
  backend is (§5); a compiled program will take the same `--backend=export`.
  It is not part of `noev build`, which only compiles: exporting executes the program.
- The export stops, without failing, where the program first needs something only running
  gives: the results of a `sample` (anything but `shots()`), or a quantum operation outside
  a `sample`. Circuits made after that point may depend on those results, so they need a
  run. A `sample` that runs shot by shot has no circuit, so it is skipped (its results are
  unknown too).
- `noev run FILE --circuits=DIR` runs the program on a real backend and saves the same
  files, including circuits that depend on earlier results.

These files are exactly what a circuit-level backend receives. `--emit=plan` says which
runs are circuits.

## 8. Open questions

- **Plug-in interface**: its versioning, so backends built separately from Noev keep working
  across releases.
- **Real-time limits**: a gate-level quantum computer will bound how long classical code may
  run between operations; the compiler would need to check those bounds for a target.
