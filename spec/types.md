# Types

Noev is statically typed. Every expression has a type known at compile time, and there are
no implicit conversions except the literal typing rules in §3.

## 1. Built-in types

| Type    | Kind      | Description                                                 |
| ------- | --------- | ----------------------------------------------------------- |
| `Int`   | classical | 64-bit signed integer                                       |
| `Float` | classical | 64-bit IEEE 754 float                                       |
| `Bool`  | classical | `true` or `false`                                           |
| `Bit`   | classical | A classical bit, usually the result of a measurement        |
| `Qubit` | quantum   | Ownership of one logical qubit                              |
| `String`| classical | Immutable text                                              |
| `()`    | classical | Unit; the type of statements and of functions with no `->` |

`Bit` and `Bool` are separate types: `Bit` is data (a measurement outcome, part of a bit
string), `Bool` is logic. Both can be used as an `if` or `while` condition (a `Bit` is true
when it is `1`). `and`, `or` and `not` work on `Bool`; `&`, `|`, `^` and `~` work on `Bit`
and `Int`. Convert between them with `Bool(b)` and `Bit(flag)`.

`String` has value semantics: assigning or passing a string gives an independent copy, which
the implementation makes cheap by sharing the text. Strings are immutable in v0.1 and can be
compared with `==` and `!=`; operations on them (length, concatenation, slicing) come later.

A `Qubit` value is not a quantum state that can be inspected. It is the right to act on one
logical qubit. The compiler and runtime decide which physical qubit that is.

## 2. Compound types

- **Tuples** `(T1, T2, …)`, indexed with `.0`, `.1`, …
- **Arrays** `[T; N]` with `N` a non-negative integer literal. Indexed with `a[i]`, where
  `i: Int`. For arrays containing qubits, every index must be proven in range at compile time
  (N018, see [ownership.md](ownership.md) §10). For classical arrays, indices proven in range
  need no check; others are checked at runtime.
- **Structs**, declared with their fields and methods together (see
  [grammar.md](grammar.md) §2). Any field may be of any type, so a struct can mix classical
  and quantum fields freely.
- **Distributions** `Distribution<T>`: how often each value of a classical type `T` occurred
  in repeated runs, made by `sample` ([execution.md](execution.md) §3). Copyable.
- **References** `ref T` (shared) and `mut T` (exclusive). See
  [ownership.md](ownership.md) §4–5. v0.1 supports elided lifetimes only; references stored
  in struct fields are Reserved.

## 3. Literals

| Literal             | Type                                                         |
| ------------------- | ------------------------------------------------------------ |
| `42`, `0xff`        | `Int`; `0` and `1` are `Bit` where a `Bit` is expected       |
| `1.0`, `1e-3`       | `Float`                                                      |
| `true`, `false`     | `Bool`                                                       |
| `\|0>`, `\|+>`, …   | `Qubit` for one character, `[Qubit; N]` for N characters      |
| `"text {x}"`        | `String`                                                     |

`let b: Bit = 1` and `if m == 1 { … }` with `m: Bit` both work through the literal rule.

A bra is not a value on its own; it only appears in a measurement test or an expectation
value (below).

### Ket literals

Each character prepares one fresh qubit:

| Char | State | Prepared as |
| ---- | ----- | ----------- |
| `0`  | \|0⟩  | allocation  |
| `1`  | \|1⟩  | `x`         |
| `+`  | \|+⟩  | `h`         |
| `-`  | \|−⟩  | `x`, `h`    |

**Ordering:** characters map to array indices left to right. `|01>` gives `r` with `r[0]`
in \|0⟩ and `r[1]` in \|1⟩, the same order as the array literal `[|0>, |1>]`. This is
reading order, not Qiskit's little-endian bit-string order; the backends translate.

A ket literal is the only way to create qubits. Because its state is known exactly, the
compiler can track it, which later allows qubits that are provably back in \|0⟩ to be
released automatically (see [ownership.md](ownership.md) §9).

The array repeat form `[|0>; N]` evaluates the ket `N` times, giving `N` independent fresh
qubits. For any other linear value, `[x; N]` is an error, since it would copy `x`.

### Measurement tests

`<s| x` measures `x` and has type `Bool`. `x` must be a `Qubit` when `s` has one character,
or `[Qubit; N]` when `s` has `N` characters. Each character picks the basis for its qubit:
`0` and `1` measure in the Z basis, `+` and `-` in the X basis. The result is `true` when
every qubit's outcome matches its character. `x` is consumed, as with `measure`.

The probability of `true` is |⟨s|ψ⟩|², so the notation means what it does in physics. When
the compiler can prove `x` is exactly in \|s⟩, the result is certainly `true`, and the
compiler may skip the measurement and release the qubits directly (see
[ownership.md](ownership.md) §9).

### Observables and expectation values

`Observable` is a classical, copyable type: a real-weighted sum of products of Pauli
operators, built with the core functions `X(i)`, `Y(i)`, `Z(i)` and `I()` and the
operators `+`, `-` and `*` (see [core.md](core.md) §4), and the standard library's projectors
`P` and `PX` ([std.md](std.md) §3).

`<ψ| H |ψ>` has type `Float`, where `H: Observable` and `ψ` is a preparation whose type is
pure quantum. Pauli indices refer to the qubits of `ψ`'s value in order: for a struct, its
fields in declaration order; for arrays and tuples, their elements. An index outside that
range is a compile error.

The value is ⟨ψ|H|ψ⟩. A simulator computes it exactly when `ψ` contains no measurement;
otherwise, and on hardware, it is estimated by averaging over repeated runs of `ψ`.

### How the two forms relate

Mathematically, a measurement test is a special case of an expectation value:
|⟨s|ψ⟩|² = ⟨ψ| (|s⟩⟨s|) |ψ⟩, the expectation of the projector onto \|s⟩. The forms differ in
what a quantum computer can physically return:

| Form          | Acts on                               | Returns                                   |
| ------------- | ------------------------------------- | ----------------------------------------- |
| `<s\| mv q`   | a qubit that exists now, consuming it | one sample of \|s⟩⟨s\|: a random `Bool`   |
| `<ψ\| H \|ψ>` | a preparation, re-run as needed      | the average of `H`: a `Float`              |

So `<11| mv r` draws once from the distribution whose probability of `true` is
`<ψ| P(0, 1) * P(1, 1) |ψ>`. The amplitude ⟨s|ψ⟩ itself cannot be observed, so Noev has no
`<s|ψ>` form: in standard notation it means the amplitude, and giving it any other meaning
would mislead.

## 4. Conversions

There is no cast operator. Conversions come in three forms, so the call site always shows
what can happen:

| Kind      | Syntax                        | Examples                                      |
| --------- | ----------------------------- | --------------------------------------------- |
| Lossless  | The target type, called       | `Float(n)`, `Bit(flag)`, `Bool(b)`, `Int(b)`  |
| Lossy     | A method named for its effect | `f.trunc()`, `f.round()`, `f.floor()`, `f.ceil()` |
| Can fail  | `T::try_from(x)`              | Reserved: needs result types                  |

`Float(n)` is shorthand for `Float::from(n)`. Once traits exist, it is the `From` trait, and
users can define conversions for their own types.

| Conversion                 | Form              | Rule                                  |
| -------------------------- | ----------------- | ------------------------------------- |
| `Int → Float`              | `Float(n)`        | Exact up to 2⁵³, rounded to nearest beyond (accepted as lossless by convention) |
| `Float → Int`              | `f.trunc()`, `f.round()`, `f.floor()`, `f.ceil()` | Saturates at the `Int` range; NaN is a runtime error |
| `Bit ↔ Bool`               | `Bool(b)`, `Bit(flag)` | `1` ↔ `true`                     |
| `Bit → Int`, `Bool → Int`  | `Int(b)`          | `0` or `1`                            |
| `Int → Bit`                | none in v0.1      | Write `if n == 0 { 0 } else { 1 }`, or wait for `Bit::try_from` |

There are no conversions to or from `Qubit`. The only way from quantum to classical is
measurement.

## 5. Linearity

Every type is **copyable**, **affine** or **linear**:

- `Int`, `Float`, `Bool`, `Bit`, `String`, `Observable`, `Distribution<T>`, `()` and shared
  references `ref T` are copyable.
- Exclusive references `mut T` are affine: they cannot be copied, but can be dropped.
- `Qubit` is linear: it cannot be copied or dropped, and must be consumed.
- A tuple, array or struct is linear if any component is linear; otherwise affine if any
  component is affine; otherwise copyable.

A type is **pure quantum** if it contains qubits and no classical data. Pure quantum places
can be lent for changing without writing `mut` (see [ownership.md](ownership.md) §4).

Linearity is a property of the type, not something the programmer declares. Readers see it
where it matters: any place a linear value changes hands has an explicit `mv`.

## 6. Traits and generics (Reserved)

Traits and generics come after v0.1, but two decisions are made now because they shape the
type checker.

**Generic code assumes values may be linear.** Rust assumes every value can be dropped, and
cannot change that without breaking existing code; it is why a Rust `Vec` of qubits would
break the must-consume guarantee. Noev takes the opposite default: a generic `T` must be
consumed like a qubit unless the code requires a trait that says otherwise:

- `T: Copy`: values can be copied (and dropped).
- `T: Discard`: values can be dropped without being consumed.

Containers and generic algorithms therefore work with qubits from the start, and generic
code that would silently drop a value is rejected where it is written, not where it is used.

**Measurement is a trait.** `Measure` pairs a quantum type with its classical result type:

```noev
trait Measure {
    type Result
    fn measure(mv self) -> Self::Result
}
```

`Qubit` measures to `Bit` and `[Qubit; N]` to `[Bit; N]`, and a user's struct can measure to
a classical struct of its own, such as `Experiment` to `ExperimentResult`. In v0.1,
`measure` is a built-in overload on `Qubit` and `[Qubit; N]`.

Other planned traits: `From` (conversions), `Copy`, `Discard`, and quantum interfaces such
as `Oracle`, `Ansatz` and `Observable`.

## 7. Printing and formatting

Every classical value can be printed and interpolated into a string. Qubits and references
to them cannot.

| Type                         | Printed as                                             |
| ---------------------------- | ------------------------------------------------------ |
| `Int`, `Float`, `Bool`       | `42`, `-0.5`, `true` (a `Float` always shows a decimal point) |
| `Bit`                        | `0` or `1`                                             |
| `[Bit; N]`, tuples of `Bit`  | A bit string in index order: `|01>` measured prints `01` |
| other tuples and arrays      | `(3, true)`, `[1, 2, 3]`                               |
| structs                      | `Experiment { angle: 0.5 }` (classical fields only)    |
| `String`                     | Its text                                               |
| `Observable`                 | `-1.0 * Z(0) * Z(1) + 0.5 * X(0)`                      |
| `Distribution<T>`            | One line per outcome, most frequent first: `11  513  51.3%` |

### Format specs

`{expr:spec}` in a string literal formats a value. The spec is a subset of Rust's:

```
spec  = [ [ fill ] align ] [ "+" ] [ "0" ] [ width ] [ "." precision ] [ type ]
align = "<" | ">" | "^"              (left, right, centre)
type  = "b" | "o" | "x" | "X" | "e"  (binary, octal, hex, HEX for Int; exponent for Float)
```

| Spec       | Applies to          | Example                          |
| ---------- | ------------------- | -------------------------------- |
| `:8`, `:>8`, `:<8`, `:^8`, `:*^8` | Any value | Pad to a width, aligned, with an optional fill character |
| `:+`       | `Int`, `Float`      | Always show the sign             |
| `:.3`      | `Float`             | Three digits after the point     |
| `:08b`     | `Int`               | Binary, zero-padded to 8 digits  |
| `:x`       | `Int`               | Hexadecimal                      |
| `:e`       | `Float`             | Scientific notation              |

A spec that does not apply to the value's type is a compile error.

## 8. Open questions

- **Classical values that are expensive to copy** (growable arrays, maps): v0.1's `String`
  and `Distribution` use value semantics with shared storage; whether growable collections do
  the same, as in Swift, is open.
- **String operations**: length, concatenation, slicing, and how indexing relates to Unicode.
- **Generic parameter syntax**: see [grammar.md](grammar.md) §4.
- **Angles.** Whether rotation angles get their own type (with units) or stay `Float`.
- **Other bases for kets**, such as `|i>` and `|-i>` for the Y basis.
