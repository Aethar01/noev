# Ownership

Ownership rules are what Noev is for: the compiler rejects programs that would copy an
unknown quantum state, use a qubit that no longer exists, or silently throw one away. This
document defines those rules. Each rule has an error code, and each code has compile-fail
tests in `tests/ui/`.

The model is Rust's (owned values, shared and exclusive references, borrow checking), with
four differences:

1. **Linear types.** A qubit must be consumed. It cannot go out of scope silently (N003).
2. **Explicit moves.** Every move is written `mv` (§3).
3. **Implicit quantum lending.** Pure quantum values are lent for changing without writing
   `mut`; classical changes stay visible (§4).
4. **Reversible contexts.** Code that must be undoable is checked for irreversible
   operations (§7).

## 1. Places and kinds of value

A **place** is a variable, optionally followed by field accesses, tuple indices and array
indices: `q`, `e.state`, `pair.0`, `r[2]`, `self.state`.

Two places **overlap** if one is a prefix of the other (`e` and `e.state`) or they are
equal. Places with different fields, tuple indices, or constant array indices are
**disjoint** (`e.state` and `e.angle`; `r[0]` and `r[1]`). Array elements with
non-constant indices are disjoint when the compiler can prove the indices differ (§10).

Every type is in one of three kinds (see [types.md](types.md) §5):

| Kind     | Types                                         | Copy? | Drop silently? |
| -------- | --------------------------------------------- | ----- | -------------- |
| Copyable | `Int`, `Float`, `Bool`, `Bit`, `()`, `ref T`  | yes   | yes            |
| Affine   | `mut T`                                       | no    | yes            |
| Linear   | `Qubit`, and anything containing one          | no    | **no**         |

References are never linear, even to a qubit: dropping a reference gives up access, not
the qubit.

A type is **pure quantum** if it contains qubits and no classical data: `Qubit`,
`[Qubit; N]`, tuples of those, and structs whose fields are all pure quantum.

## 2. Mutability

`let` and `let mut` control **classical** data:

- Reassigning a variable, or changing any classical data in it, needs `let mut`.
- Lending a place with `mut` needs `let mut`, unless the place is pure quantum.
- Quantum state can always be changed by gates. A qubit's state is not a value the program
  can see, so "immutable qubit" would mean nothing. `let q = |0>` followed by `q.h()` is
  fine.

```noev
let e = Experiment::new(0.5)
e.state.h()             // ok: `e.state` is pure quantum
e.angle = 1.0           // error[N015]: `e` is not declared `let mut`
e.rotate()              // error[N015]: `rotate` takes `mut self`, and `e` has classical data
```

## 3. Moves: `mv`

**Every move is written `mv`, and nothing else moves.** A variable can only become unusable
at a visible `mv`.

A value moves when it is:

- used to initialise or assign a variable: `let b = mv a`
- passed to a parameter of owned type: `measure(mv q)`, `consume(mv e)`
- used as a `mv self` receiver: `mv q.measure()`, `mv e.finish()`
- put into a struct, tuple or array: `Pair { a: mv q, b: mv r }`, `(mv q, mv r)`
- returned: `return mv q`, or a block's final expression `mv q`

For linear and affine values, `mv` is required at these points. For copyable values it is
optional and means the same thing: the variable ends here, and the value is handed over
without a copy. Using it afterwards is an error (N002). Without `mv`, copyable values are
copied.

Fresh values (ket literals, call results, struct literals) are not places, so they need no
`mv`: `let q = |0>`, `let b = mv q.measure()`.

## 4. Lending: `ref` and `mut`

`ref place` lends a place to be read; `mut place` lends it to be changed. While a `ref` is
in use, nothing may change the place. While a `mut` is in use, the place may not be used in
any other way (§5).

Where the expected type is a reference, the compiler lends implicitly in two cases:

| Expected type | Argument `x` of type `T`           | Implicit?                         |
| ------------- | ---------------------------------- | --------------------------------- |
| `ref T`       | any `T`                            | yes: `describe(e)` means `describe(ref e)` |
| `mut T`       | pure quantum `T`                   | yes: `h(q)` means `h(mut q)`      |
| `mut T`       | `T` containing classical data      | **no**: write `bump(mut count)` (N016) |

Method receivers are always lent implicitly (`e.rotate()`), except `mv self`, which needs
`mv`.

These shortcuts are always unambiguous, because moves are always written `mv`: a bare name
can never mean a move. Writing `ref` or `mut` where it could be left out is allowed.

A variable holding a reference stands for the value it refers to. `n += 1` with
`n: mut Int` changes the caller's integer, and `r.x()` with `r: mut Qubit` acts on the
caller's qubit. To lend a reference on, use `ref r` or `mut r` (or nothing, under the rules
above).

**Shared references to qubits.** All gates need `mut Qubit`. A `ref Qubit` allows reading
classical data beside it and using the qubit as a `ctrl` control (§6), but no gates and no
measurement. "No operation is applied *through* this reference" is what `ref` guarantees, not
"this qubit's state cannot change": under `ctrl`, a control can pick up a relative phase
(phase kickback).

## 5. How long a lend lasts

**v0.1 uses lexical lifetimes:** a lend stored in a variable lasts until the end of the
block that declares the variable; a lend that is only used within one statement (a call
argument, a method receiver, a chain) lasts until the end of that statement.

This is stricter than Rust's non-lexical lifetimes, which end a lend at its last use. Moving
to non-lexical lifetimes later only accepts more programs, so it breaks nothing.

References in function signatures follow Rust's elision rules: a returned reference borrows
from the receiver if there is one, otherwise from the single reference parameter. Signatures
that need anything else, and references stored in struct fields, need explicit lifetimes,
which are Reserved.

## 6. Rules

### N001: move without `mv`

```noev
let a = |0>
let b = a               // error[N001]: qubits cannot be copied
                        //   help: write `let b = mv a` to move it
```

Applies to every move point in §3.

### N002: use after move

```noev
let q = |0>
let b = mv q.measure()
q.x()                   // error[N002]: `q` was moved by `measure` on line 2
```

Applies to any use of a place that overlaps a moved place.

### N003: linear value not consumed

```noev
fn f() {
    let q = |+>
    q.h()
}                       // error[N003]: `q` is still owned when it goes out of scope
                        //   help: measure it with `mv q.measure()`, release it with
                        //   `mv q.discard()`, or return it
```

At the end of every scope, and at every `return` and `break`, each linear place owned by
that scope must have been moved. A qubit is consumed by `measure`, `discard`, moving it into
something that is itself consumed, or returning it. Discarding a linear value as a block's
unused final expression is also N003.

There is never an implicit measure or discard. If `q` is entangled with a qubit the program
keeps, discarding `q` acts as a measurement and collapses the kept qubit. The programmer has
to see that happen.

### N004: `mv` where a reference is expected

```noev
h(mv q)                 // error[N004]: `h` lends its argument; remove `mv`
```

### N005: conflicting lends

```noev
swap(a, a)              // error[N005]: `a` is lent for changing twice

let r = q.h()           // r lends q until the end of the block
q.s()                   // error[N005]: `q` is already lent to `r`
```

A place may have either one `mut` lend or any number of `ref` lends in use at a time, and
overlapping places count as the same place. `swap(e.a, e.b)` is fine because the fields
are disjoint. `ctrl` controls are `ref` lends for the duration of the block (§8).

### N006: move while lent

```noev
let r = ref q
let b = mv q.measure()  // error[N006]: `q` is moved while lent to `r`
```

### N007: move out of a reference

```noev
fn bad(q: mut Qubit) -> Bit {
    mv q.measure()      // error[N007]: cannot move the qubit out of `q`, a reference
}                       //   help: take ownership with `q: Qubit`
```

A reference gives access, not ownership, so nothing reachable through it can be moved. This
also covers chains ending in a consuming call, such as `q.h().measure()`, since `h` returns
`mut Qubit`.

### N008: reference outlives its value

```noev
fn make() -> mut Qubit {
    let q = |0>
    mut q               // error[N008]: `q` is dropped at the end of `make`
}
```

### N009: moved in only some branches

```noev
let q = |+>
if flag { mv q.discard() }
                        // error[N009]: `q` is moved when `flag` is true but not otherwise
```

After an `if`, every linear place must be moved in all branches or in none. An `if` without
`else` counts as having an empty `else`. Otherwise the program could not know whether the
qubit still needs consuming.

The right operand of `and` and `or` only runs when the left one does not decide the result,
so it counts as a branch too:

```noev
let q = |0>
let both = flag and <1| mv q
                        // error[N009]: `q` is only moved when the right side of `and` runs
                        //   help: measure it first, in its own `let`
```

### N010: moved inside a loop

```noev
let q = |0>
for i in 0..3 { mv q.discard() }    // error[N010]: `q` would be moved on every iteration
```

### N011: partially moved value used as a whole

```noev
let mut e = Experiment::new(0.5)
let b = mv e.state.measure()
let a = e.angle         // ok: `angle` was not moved
e.rotate()              // error[N011]: `e.state` was moved, so `e` cannot be used as a whole
```

Moving a field leaves the other fields usable one by one. If every remaining field is
copyable, the value no longer needs consuming (N003 does not apply).

### N012: moving an array element with a non-constant index

```noev
let r = |000>
let b = mv r[i].measure()   // error[N012]: cannot move out of `r[i]` with a non-constant index
```

The checker must know which elements are gone, so elements can only be moved with constant
indices. Lending with any index is fine.

### N015: change without `let mut`

See §2. Applies to assignment, compound assignment, and `mut` lends of places containing
classical data.

### N016: classical value lent for changing without `mut`

```noev
fn bump(n: mut Int) { n += 1 }
bump(count)             // error[N016]: `bump` changes `count`; write `bump(mut count)`
```

## 7. Reversible contexts

The body of a reversible function, the body of a `ctrl` block, and the `with` block of
`with`/`do` are reversible contexts. Code in them must be undoable, so the compiler can
control it, invert it with `adj`, and uncompute it.

**A function is reversible if its body satisfies the rules below.** The compiler infers this;
nothing needs to be written. Declaring `rev fn` turns the inference into a promise: the
compiler reports N013 inside the function if its body stops being reversible, rather than at
some distant caller. Once Noev has modules, functions visible outside their module must state
`rev` explicitly to be usable as reversible ([modules.md](modules.md) §5).

### N013: irreversible operation in a reversible context

```noev
rev fn bad(q: mut Qubit) -> Bit {
    mv q.measure()      // error[N013]: `measure` is not reversible
}

fn coin(q: mut Qubit) { q.reset() }
ctrl a { coin(b) }      // error[N013]: `coin` is not reversible
                        //   note: it calls `reset` on line 1
```

In a reversible context, these are not allowed:

- calls to `measure`, `reset`, `discard`, or any function that is not reversible;
- ket literals (allocating qubits), except in a `with` block;
- moving a linear value;
- assignment to a variable declared outside the context.

Allowed: gates, calls to reversible functions (including purely classical ones, which are
trivially reversible), nested `ctrl`, `adj`, `let` bindings and assignments of variables
declared inside the context, `if` and `while`, and `for` loops over classical ranges.

Classical control flow is safe here because nothing in a reversible context can depend on a
measurement: every classical value is determined by the inputs, so undoing the code can
recompute it and replay the quantum operations in reverse.

### N014: `adj` on something that is not reversible

```noev
adj q.measure()         // error[N014]: only reversible functions can be inverted
```

`adj f(args)` runs `f` backwards: its operations in reverse order, each inverted. Loops run
in reverse.

## 8. Quantum control

```noev
ctrl a, b { c.x(); d.h() }
```

The controls are `ref`-lent for the duration of the block. Inside it, they cannot be lent
for changing or moved, so applying a gate to a control is N005:

```noev
ctrl a { a.x() }        // error[N005]: `a` is a control of this block
ctrl r { r[0].x() }     // error[N005]: `r[0]` overlaps the control `r`
```

A nested `ctrl` may add further controls (`ctrl a { ctrl b { … } }`). A control may be any
pure quantum value (a qubit, an array or tuple of qubits, a struct of qubits) or a reference
to one; every qubit in it is a control. So `ctrl r { t.x() }` with `r: [Qubit; 3]` is a
multi-controlled X. The body is a reversible context (§7).

### `with` / `do` (Reserved)

```noev
with { majority(a, b, c) }
do   { ctrl c { out.x() } }
```

Runs the `with` block, then the `do` block, then the inverse of the `with` block. Qubits
allocated in `with` are back in \|0⟩ at the end and are released automatically. During
`do`, every qubit the `with` block allocated or acted on is `ref`-lent (usable only as a
control). This guarantees the inverse restores them.

Parsed in v0.1, rejected with "not yet supported".

## 9. Toward automatic release (goal, not v0.1)

v0.1 requires every qubit to be consumed explicitly (N003). The goal is to release a qubit
automatically when the compiler can **prove** it is back in \|0⟩: for example, a qubit never
acted on since its `|0>` literal, or one restored by `with`/`do`. Accepting those programs
later breaks no existing code. `discard` stays available for saying it explicitly.

So the checker records more than "owned / moved / lent" per place from the start: at least
whether a qubit has been acted on since it was created, and its literal state if not.

## 10. Array indices

Quantum code constantly acts on several elements of one array at once (`r[i]` and
`r[i + 1]` in a ladder, `r[i]` and `r[j]` in a QFT). Whenever two lends of the same array are
in use together (N005), the compiler must prove their indices differ. It does this at compile
time, with no runtime checks.

### Index facts

Each index is put in linear form: a sum of integer variables times constants, plus a
constant (`2*i + 1`, `j - 1`, `n - 1 - i`). Indices outside this form (`r[f(i)]`, `r[i * j]`)
cannot be compared, except with an identical expression.

The compiler knows these facts at each point in the program:

- **Loop bounds.** Inside `for v in a..b`, `a ≤ v` and `v ≤ b - 1`, where `a` and `b` are
  their values when the loop started. Loop variables cannot be assigned, and loop bounds are
  evaluated once, so these facts hold for the whole body.
- **Array lengths.** Indices into `[T; N]` lie in `0..N` (enforced by the bounds rule below).
- **`if` conditions.** Inside the `then` block of `if c`, the comparisons in `c` hold (and in
  the `else` block, their negation when `c` is a single comparison). Comparisons combined
  with `and` each hold.
- **`assert`.** After `assert c`, the comparisons in `c` hold. `assert` is checked at
  runtime: if it fails, the program stops with an error before the code after it runs.

Only comparisons between linear-form expressions (`==`, `!=`, `<`, `<=`, `>`, `>=`) become
facts. A fact stops holding when any variable in it is assigned.

### Proving two indices differ

For indices `x` and `y`, the compiler takes `d = x - y` and:

1. if `d` is a non-zero constant, they differ (`r[i]` and `r[i + 1]`; `r[2*i]` and
   `r[2*i + 1]`);
2. otherwise, if `d` has the form `v - w + c` (or `v + c`), it uses the facts to bound `d`,
   by shortest paths over the difference constraints (the standard technique for removing
   bounds checks). If `d ≥ 1` or `d ≤ -1` follows, they differ;
3. otherwise, if a fact `x != y` (or `y != x`) holds for these exact linear forms, they
   differ;
4. otherwise, they are not proven different: error N017.

```noev
for i in 0..n-1 { ctrl r[i] { r[i + 1].x() } }                          // ok: differ by 1
for i in 0..n/2 { swap(r[2*i], r[2*i + 1]) }                             // ok: differ by 1
for i in 0..n { for j in i+1..n { ctrl r[j] { r[i].p(angle(i, j)) } } }  // ok: j ≥ i + 1
```

### N017: indices not proven different

```noev
for i in 0..n { swap(r[i], r[n - 1 - i]) }
                        // error[N017]: cannot prove `r[i]` and `r[n - 1 - i]` are different
                        //   note: they are equal when i = (n - 1) / 2
                        //   help: loop over `0..n/2` instead
```

The error shows both indices, and a value where they coincide when one is easy to find. When
the programmer knows more than the compiler, an `assert` or `if` supplies the fact:

```noev
for i in 0..n {
    if i != n - 1 - i { swap(r[i], r[n - 1 - i]) }    // ok: fact from the condition
}
```

This analysis only concerns lending. Moving an element still requires a constant index
(N012).

### Bounds

The same facts prove that indices are in range. Each variable's range comes from the facts,
and the index's range follows from its linear form by interval arithmetic: with `i` in
`0..3`, `2*i + 1` lies between 1 and 5. Indexing a qubit array (or any array
containing qubits) must be proven in range at compile time; otherwise it is N018. Indexing a
purely classical array that cannot be proven in range gets a runtime check instead (see
[types.md](types.md) §2).

### N018: qubit index not proven in range

```noev
let r = |0000>
for i in 0..4 { ctrl r[i] { r[i + 1].x() } }
                        // error[N018]: `i + 1` can be 4, outside `r` (length 4)
                        //   help: loop over `0..3`, or `assert i + 1 < 4`
```

A runtime out-of-range failure halfway through a circuit, with qubits entangled, is as bad
as an ownership failure, so it is ruled out at compile time.

### Later

`split`-style functions for unusual patterns, and facts from more sources (such as `while`
conditions), can be added later without breaking anything.

## 11. Unsafe

`assert` covers claims that can be checked at runtime. `unsafe` is for claims that cannot be
checked at all:

> Inside `unsafe`, the compiler trusts a claim it cannot verify. If the claim is false, the
> program still runs, but its **physical results may be wrong**.

Noev has no raw memory, so unlike Rust, `unsafe` never leads to undefined behaviour in the
memory sense. What it risks is a wrong quantum computation.

### Unsafe operations

| Operation                    | Claim                             | Status   |
| ---------------------------- | --------------------------------- | -------- |
| `q.release()`                | `q` is in \|0⟩, so it can be released without a reset | v0.1 |
| Calling an `unsafe fn`       | Whatever that function documents  | v0.1     |
| Reversible functions the checker cannot verify, such as measurement-based uncomputation, with hand-written inverse and controlled versions | They act as the stated unitary | Reserved |
| Calling C functions and inline OpenQASM ([interop.md](interop.md)) | They behave as declared | Reserved |
| Choosing physical qubits     | The chosen qubits are valid on the target | Reserved |

`release` exists because a reset costs time and adds errors on hardware. Once the compiler
can prove a qubit is back in \|0⟩ (§9), it releases it without being asked; `release` covers
the cases it cannot prove yet.

**Simulators check unsafe claims.** When running on a simulator, `release` checks that the
qubit really is in \|0⟩ and reports an error if not. On hardware the claim is trusted. So
unsafe code can be tested.

### What `unsafe` never allows

These are facts about the physics or about ownership, so there is no claim to vouch for:

- copying a qubit (N001);
- using a value after it is moved (N002);
- dropping a qubit without consuming it (N003); `release` is itself a consuming call;
- two lends of the same qubit for changing at once (N005, N017).

All other rules apply unchanged inside `unsafe`.

### N019: unsafe operation outside `unsafe`

```noev
mv q.release()          // error[N019]: `release` is unsafe: it trusts that `q` is in |0⟩
                        //   help: use `mv q.discard()`, which resets first, or wrap in `unsafe { … }`
```

`unsafe fn` declares a function whose callers must uphold a claim; calling it outside
`unsafe` is N019. The body of an `unsafe fn` is not itself an `unsafe` block; unsafe
operations inside it still need one, so each claim is visible where it is made.

## 12. Bras and preparations

A measurement test `<s| x` consumes `x` like `measure`: a place needs `mv` (N001), and it is
not allowed in a reversible context (N013).

An expectation value `<ψ| H |ψ>` runs the preparation `ψ` as many times as the estimate
needs, each time creating and consuming its qubits internally. So `ψ` must be repeatable:

### N020: preparation is not repeatable

```noev
let q = |+>
let e = <prep(q)| Z(0) |prep(q)>     // error[N020]: `prep` is run many times, but `q` exists once
                                     //   help: create the qubits inside `prep`
```

The arguments of a preparation must be classical (copyable) values: no qubits, no `mv`, and
no `mut` lends. A ket literal is always repeatable. Expectation values are not allowed in a
reversible context (N013).

## 13. Open questions

None currently.

## 14. Error code index

| Code | Error                                              |
| ---- | -------------------------------------------------- |
| N001 | Move without `mv`                                  |
| N002 | Use after move                                     |
| N003 | Linear value not consumed                          |
| N004 | `mv` where a reference is expected                 |
| N005 | Conflicting lends (including `ctrl` controls)      |
| N006 | Move while lent                                    |
| N007 | Move out of a reference                            |
| N008 | Reference outlives its value                       |
| N009 | Moved in only some branches                        |
| N010 | Moved inside a loop                                |
| N011 | Partially moved value used as a whole              |
| N012 | Moving an array element with a non-constant index  |
| N013 | Irreversible operation in a reversible context     |
| N014 | `adj` on something that is not reversible          |
| N015 | Change without `let mut`                           |
| N016 | Classical value lent for changing without `mut`    |
| N017 | Indices not proven different                       |
| N018 | Qubit index not proven in range                    |
| N019 | Unsafe operation outside `unsafe`                  |
| N020 | Preparation is not repeatable                      |
| N021 | A `circuit fn` is not a circuit ([execution.md](execution.md) §4) |
| N022 | A repeated run prints ([execution.md](execution.md) §3) |
