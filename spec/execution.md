# Execution

How Noev programs run: what one run is, how runs are repeated, and when a run is a
**circuit** that can be sampled instead of run shot by shot. The rule is meant to be checked
by reading the code: you should always be able to tell which way something will run.

Classical and quantum code are equally part of a program. Nothing here sets up a separate
"host" language around quantum "kernels": every function is ordinary Noev, and where each
piece runs (CPU, GPU, or a quantum computer's control system) is the compiler's decision.

## 1. Every call runs once

Calling a function runs it once, whether it is classical, quantum or both. Calling a
function that measures qubits gives one outcome, which is what a quantum computer does: a
quantum computer cannot copy a state or sample one state twice, so every repetition is a
complete new run.

`main` is no different: a program runs `main` once, like a program in any other language.
`noev run` runs it, and the program's output is whatever it prints (§6).

## 2. Closed runs

A **closed run** is a piece of code that takes no qubits from outside it: a `sample` block,
or a call whose parameters and result contain no qubits (and no references). Ownership
guarantees that every qubit it creates is consumed inside it (or, for a `sample` block,
returned to be measured), so it is a self-contained quantum computation that can be repeated
independently: one **shot**.

```noev
fn bell() -> (Bit, Bit) { … }            // closed: no qubits in or out
fn ansatz(theta: Float) -> [Qubit; 2] { … }   // not closed: returns qubits
fn flip(q: mut Qubit) { … }              // not closed: takes a qubit
```

Closed calls are found from function signatures alone.

## 3. Repeating runs

Runs are only ever repeated explicitly:

- `sample n { … }` runs its block `n` times and returns a `Distribution` of its results.
- `<ψ| H |ψ>` runs its preparation as many times as the estimate needs
  ([types.md](types.md) §3).
- An ordinary loop that calls a run repeats it, one run per call.

### `sample`

```noev
fn main() {
    let d = sample 1000 { bell() }
    print(d)                                    // every outcome, most frequent first
    print("P(11) = {d.probability((1, 1)):.3}")

    let theta = 0.3
    let e = sample 500 {                        // any block is a run
        let q = |0>
        q.ry(theta)
        mv q.measure()
    }
}
```

`sample` is a block, like `for` or `while`, so the code still reads in order: `sample n
{ … }` means "run this block `n` times and count what it gives", exactly as a loop that
collects its results would. A call inside the block runs once per run, where it is written.

`sample n { … }`:

- `n` is an `Int`, at least 1. It is evaluated once, before the runs.
- Each run is separate: it starts with no qubits, and is given a copy of the classical values
  it reads from the code around it. It may not use qubits or references from outside it,
  change outside variables, or lend them `mut` (N020). `return`, `break` and `continue` cannot
  leave it.
- The block's value is the run's result. Qubits in it are measured in the Z basis at the end
  of each run, in the order of the value's qubits (elements and fields left to right). So
  `sample 1000 { ansatz(t) }` samples the state `ansatz` prepares. The `Distribution`'s
  element type is the block's type with every `Qubit` replaced by `Bit`.
- Every other part of the result must be classical; references are not allowed.

A `Distribution<T>` has:

| Method                          | Result                                     |
| ------------------------------- | ------------------------------------------ |
| `d.shots() -> Int`              | How many runs it counts                    |
| `d.count(x: T) -> Int`          | How many runs gave `x`                     |
| `d.probability(x: T) -> Float`  | `count(x) / shots()`                       |
| `d.most_likely() -> T`          | The most frequent result (the smallest one, on ties) |

Printing a distribution lists every outcome with its count and percentage, most frequent
first, with ties in increasing order: `11  513  51.3%`.

Repetitions of a closed run are independent, so a compiler or runtime may perform them
together: one job with `n` shots on a quantum computer, or one simulation sampled `n` times
on a simulator, when the run is a circuit (§4).

### Repeated runs have no output

A run that is repeated (a `sample` block, or a preparation) must not print, directly or in any
function it calls: it would print once per run, and when it is a circuit it is not run once
per shot at all. Printing in a repeated run is error N022. Compute values in the run and
print them after `sample`.

## 4. Circuits

A closed run is a **circuit** when it can be written as a quantum circuit with classical
control. Then it is computed once and sampled for every shot, on a simulator, and sent as a
single job, on hardware. The rule:

> **A run is a circuit if, until its last quantum operation, its measurement results are
> only used in `if` and `while` conditions or put into its result. After its last quantum
> operation, measurement results are ordinary data.**

In detail:

- A **measurement result** is the `Bit` from `measure`, the `Bool` from a measurement test
  (`<s| x`), and anything computed from them.
- Before the last quantum operation, a measurement result may only be:
  - a condition of `if` or `while`, including comparisons and logic on measurement results
    and constants (`==`, `!=`, `and`, `or`, `not`, `&`, `|`, `^`, `~`, and the conversions
    `Bool(b)` and `Bit(flag)`);
  - stored in variables, and moved or copied into the run's result, directly or inside
    tuples, arrays and structs.

  Any other use (arithmetic producing an `Int` or `Float`, an argument to a function, an
  array index, a gate's angle) makes the run not a circuit.
- **Classical code that does not depend on measurement results is unrestricted**: loops,
  arithmetic, structs, calls to helper functions, angles computed from arguments. It runs
  while the circuit is being built.
- After the last quantum operation (the point after which no path through the run applies
  a gate, measures, resets or allocates, including inside calls), anything goes: counting
  ones, decoding, building a result.
- Calls to other functions inside a run are part of it: their quantum operations are part of
  the circuit, and passing a measurement result to them counts as "any other use".

```noev
fn a() -> (Bit, Bit) { …; (mv x.measure(), mv y.measure()) }        // circuit: results returned
fn b() -> Bit { …; if mv x.measure() { y.x() }; mv y.measure() }    // circuit: result in a condition
fn c() -> Int {                                                      // circuit: arithmetic after
    …                                                                //   the last quantum operation
    let (p, q) = (mv x.measure(), mv y.measure())
    Int(p) + Int(q)
}
fn d() -> Bit {                                                      // not a circuit: a result
    …                                                                //   feeds a gate's angle
    let k = Int(mv x.measure())
    y.ry(Float(k))
    mv y.measure()
}
```

A run that is not a circuit is still correct; it runs shot by shot.

### `circuit fn`

Whether a function is a circuit is inferred. Declaring `circuit fn` turns the inference into a
promise, exactly as `rev fn` does for reversibility: if the body is not a circuit, the
compiler reports N021 at the function, rather than the program silently running shot by
shot.

```noev
circuit fn d() -> Bit {
    let x = |+>
    let y = |0>
    let k = Int(mv x.measure())
    y.ry(Float(k))      // error[N021]: `d` is declared `circuit fn`, but a measurement result
    mv y.measure()      //   is used in a gate's angle before the last quantum operation
}
```

`circuit fn` may only be declared on a closed run's function. Once Noev has modules, a `pub`
function is a circuit to other modules only if it is declared `circuit fn`
([modules.md](modules.md) §5).

## 5. What the compiler reports

Which way each repeated run executes is decided at compile time and reported then, never in
the program's output:

- `noev check` and `noev run` warn, on standard error, when a `sample` block or a preparation
  will run shot by shot, pointing at the line that makes it so.
- `noev build --emit=plan` lists every `sample` block and how it runs, then every function
  with quantum operations and whether it is a circuit, and why not when it is not.
- `noev run --circuits=DIR` saves every circuit a run sends to its backend, as OpenQASM 3
  ([runtime.md](runtime.md) §7).

The current compiler turns part of the rule into circuits. An `if` on a measurement result
becomes an `if` in the circuit when its branches only apply gates (directly, through `ctrl`,
or by calling functions that only apply gates) and compute classical values of their own. A
circuit that needs more runs shot by shot for now, with the warning saying why:

- a `while` whose condition depends on a measurement result;
- a branch on a measurement result that creates qubits, measures, frees qubits, prints,
  changes a variable from outside the branch, or leaves it early (`return`, `break`);
- an `if` on a measurement result that gives a value (`let x = if m { … } else { … }`).

One more case runs shot by shot on a simulator: a run that calls `release`. The simulator
checks `release`'s claim that the qubit is in \|0⟩ ([ownership.md](ownership.md) §11), which
needs each shot's state. The warning says so; `discard`, which resets first, keeps the run
sampled.

Optimisations that only make the fast path faster are not reported and never change which
path is taken: caching a function's circuit and rebinding its angles on each call, running
post-processing once per distinct outcome, and the simulator's strategy for runs that are
not circuits.

## 6. Running `main`

`noev run` runs `main` once, as any language runs a program:

- `main` takes no parameters and returns `()`.
- Standard output is exactly what the program prints; nothing is added.
- A failure (an `assert`, a runtime error such as division by zero) is reported on standard
  error, and the exit status is non-zero.
- `--seed N` fixes the simulator's random numbers, so a run can be reproduced.

## 7. On quantum hardware (not v0.1)

- A circuit becomes a job: the compiler emits it (as QIR or OpenQASM 3) with its classical
  control, and the quantum computer runs it for every shot.
- Classical code before a circuit (computing its angles, sizes and structure) and after it
  (post-processing) runs on ordinary processors.
- A run that is not a circuit needs its classical code to run on the quantum computer's
  control system between operations, in real time. What is possible there depends on the
  hardware; the compiler will reject what a target cannot do, and report it.

## 8. Error codes

| Code | Error                                                       |
| ---- | ----------------------------------------------------------- |
| N021 | A `circuit fn` is not a circuit, or is not a closed run      |
| N022 | A repeated run prints                                       |

## 9. Open questions

- **More on `Distribution`**: iterating over outcomes, marginals, and combining
  distributions, once Noev has iteration and generics.
- **Measurement results in `while` conditions**: allowed by the rule, but a loop that runs
  until a measurement succeeds has no bound on its length. Simulators handle it; hardware
  targets may need a bound.
