# Types

Noev is statically typed. Every expression has a type known at compile time, and there are
no implicit conversions except the literal typing rules in §3.

## 1. Built-in types

| Type    | Kind      | Description                                                 |
| ------- | --------- | ----------------------------------------------------------- |
| `Int`   | classical | An integer of any size: never overflows                     |
| `Float` | classical | 64-bit IEEE 754 float, like Python's `float`; also called `f64` |
| `Bool`  | classical | `true` or `false`                                           |
| `Bit`   | classical | A classical bit, usually the result of a measurement        |
| `Qubit` | quantum   | Ownership of one logical qubit                              |
| `String`| classical | Immutable text                                              |
| `()`    | classical | Unit; the type of statements and of functions with no `->` |
| `i8` `i16` `i32` `i64` | classical | Signed integers of that many bits                 |
| `u8` `u16` `u32` `u64` | classical | Unsigned integers of that many bits               |
| `f32`   | classical | 32-bit IEEE 754 float                                       |
| `Complex` | classical | A complex number of two `Float`s, like Python's `complex`; also called `c64` |
| `c32`   | classical | A complex number of two `f32`s                              |

`Int` grows as needed, so results such as `1 << 100` and literals such as
`123456789012345678901234567890` are exact. Values that fit in 64 bits, almost all of them in
practice, are as fast as a machine integer. Its operators:

- `/` truncates toward zero and `%` takes the sign of the dividend, as in Rust and C; dividing
  by zero is a runtime error.
- `&`, `|`, `^` and `~` act as on an endless two's-complement number, so `~0 == -1`; `>>`
  rounds toward negative infinity. Shifting by a negative amount is a runtime error.
- Where a machine-sized number is needed (an array index, a shot count) a value out of that
  range is a runtime error.

**Sized numbers** are for when the size matters: data with a fixed layout, a measured
register read as a number, a host function's arguments. `Int` and `Float` are the defaults,
as `int` and `float` are in Python, and each sized type is its own type, mixed with no other
without a conversion (§4); `f64` is not a sized type but another name for `Float`. Arithmetic
on sized integers follows `Int`'s rules, except that a result outside the type's range is a
runtime error (`x + 1` fails when `x: u8` is 255; there is no silent wrapping). Shifts drop
the bits shifted out, as in Rust and C, and shifting by the type's width or more is an error.
`~` flips every bit of the type, so `~0` as a `u8` is `255`.

**Complex numbers** are written with imaginary literals, a number with `i` directly after
it: `1.0 + 2.0i`, `Complex::polar(1.0, theta)` for a phase. `i` alone is an ordinary name.
`+`, `-`, `*`, `/`, unary `-`, `==` and `!=` work on two values of the same complex type;
there is no ordering. Their methods: `re()` and `im()` (the parts, as `Float` or `f32`),
`abs()` and `arg()` (modulus and phase), `conj()`, `exp()` and `sqrt()` (the principal
root), and the associated function `polar(r, theta)`. As everywhere, nothing converts
implicitly: `z * 2.0` works because the literal takes the type expected, but `z * x` with
`x: Float` needs `Complex(x)`. A complex number prints as `1.0+2.0i`.

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
- **Enums**: a value that is one of several variants, each with its own data (below).
- **Type aliases** `type Byte = [Bit; 8]`: another name for a type, interchangeable with it
  everywhere (below).
- **Distributions** `Distribution<T>`: how often each value of a classical type `T` occurred
  in repeated runs, made by `sample` ([execution.md](execution.md) §3). Copyable.
- **References** `ref T` (shared) and `mut T` (exclusive). See
  [ownership.md](ownership.md) §4–5. v0.1 supports elided lifetimes only; references stored
  in struct fields are Reserved.

### Enums

An enum is a value that is exactly one of its **variants**. A variant can hold nothing, a
list of values, or named fields:

```noev
enum Basis { X, Y, Z }

enum Outcome {
    Success(Bit),
    Retry { attempts: Int },
    Failed,

    fn succeeded(ref self) -> Bool {
        match self {
            Outcome::Success(_) => true,
            _ => false,
        }
    }
}
```

- Variants are written with the enum's name, as types and namespaces are: `Basis::X`,
  `Outcome::Success(1)`, `Outcome::Retry { attempts: 3 }`. A variant with values is also a
  function: `Outcome::Success` takes a `Bit` and gives an `Outcome`.
- Methods and associated functions go inside the enum, after the variants, as in a struct.
- A value is taken apart with `match` ([grammar.md](grammar.md) §2), which must cover every
  variant.
- Variants can hold any type, qubits included. An enum is linear, affine or copyable by its
  variants' data (§5), and ownership follows from that ([ownership.md](ownership.md) §13).
- Enums whose data can be compared can be compared with `==` and `!=`: same variant and
  equal data. So can arrays and tuples whose elements can be, element by element:
  `m == [0, 1]` for measured bits, `(a, b) != (0, 0)`. Comparing measurement results this way
  is logic on them, which a circuit can use ([execution.md](execution.md) §4).

**Type aliases** name a type that is long or used often:

```noev
type Byte = [Bit; 8]
type Register = [Qubit; 8]
type Reading = Result<Byte, String>

fn read(r: Register) -> Byte { mv r.measure() }
```

An alias is the same type as what it names, not a new one: a `Byte` is a `[Bit; 8]`, passes
where one is expected, and is linear, copyable or neither exactly as that type is. It can name
any type, including another alias, but not itself, directly or through others. An alias of a
struct or enum also names its variants, associated functions and literals (`R::new()`,
`R { … }`). Error messages show the type an alias names. For a type that is new, so that it
cannot be mixed up with what it holds, declare a struct.

**`Option` and `Result`** are built-in enums until Noev has generics (§6), like
`Distribution<T>`:

| Type           | Variants              | For                                       |
| -------------- | --------------------- | ----------------------------------------- |
| `Option<T>`    | `Some(T)`, `None`     | A value that may be absent                |
| `Result<T, E>` | `Ok(T)`, `Err(E)`     | The result of something that can fail      |

Their variants are in scope without the enum's name: `Some(q)`, `None`, `Ok(x)`, `Err(e)`,
and print that way. `Some(x)` is an `Option` of `x`'s type; `None`, `Ok` and `Err` take their
types from where they are used (a declared type, a parameter, a return type, an earlier
branch), so `let x = None` alone is an error: write `let x: Option<Int> = None`.
`T` and `E` may be any type, so `Option<Qubit>` is "a qubit, or none", and the compiler
makes sure a `Some` qubit is consumed.

## 3. Literals

| Literal             | Type                                                         |
| ------------------- | ------------------------------------------------------------ |
| `42`, `0xff`        | `Int`, or the sized integer expected; `0` and `1` are `Bit` where a `Bit` is expected |
| `1.0`, `1e-3`       | `Float`, or `f32`, `Complex` or `c32` where one is expected  |
| `2.0i`, `3i`        | `Complex`, or `c32` where one is expected                    |
| `true`, `false`     | `Bool`                                                       |
| `\|0>`, `\|+>`, …   | `Qubit` for one label, `[Qubit; N]` for N labels             |
| `"text {x}"`        | `String`                                                     |

`let b: Bit = 1` and `if m == 1 { … }` with `m: Bit` both work through the literal rule, as
do `let x: u8 = 200` and `x + 1` with `x: u8`. A literal outside its type's range is a
compile-time error (`let x: u8 = 256`).

A bra is not a value on its own; it only appears in a measurement test or an expectation
value (below).

### Ket literals

Each label prepares one fresh qubit. A label is one character, or a longer one in
parentheses:

| Label  | State  | Basis | Prepared as   |
| ------ | ------ | ----- | ------------- |
| `0`    | \|0⟩   | Z     | allocation    |
| `1`    | \|1⟩   | Z     | `x`           |
| `+`    | \|+⟩   | X     | `h`           |
| `-`    | \|−⟩   | X     | `x`, `h`      |
| `(+i)` | \|+i⟩  | Y     | `h`, `s`      |
| `(-i)` | \|−i⟩  | Y     | `x`, `h`, `s` |

So `|(+i)>` is one qubit and `|0(-i)+>` three. The parentheses keep "one label, one qubit"
readable: `|-i>` would be two qubits, \|−⟩ and an unknown `i`.

**Computed labels.** A label can also be computed, written `{expr}` as in string
interpolation, and mixed with fixed ones: `|0{b}+>`. Its value decides the labels:

| Value         | Labels                                                       |
| ------------- | ------------------------------------------------------------ |
| `Bit`         | one: `0` or `1`                                              |
| `[Bit; N]`    | N, one per bit: `\|{bits}>` prepares a register from data     |
| `State`       | one, any of the six ([std.md](std.md) §2): `State::PlusI` is `(+i)` |
| `[State; N]`  | N                                                            |

The number of qubits still follows from the types, so `|0{bits}>` with `bits: [Bit; 3]` is a
`[Qubit; 4]`. A label computed from a measurement result prepares the qubit conditionally, as
`if m { q.x() }` would, so a circuit can still use it. In a bra, each computed label picks
its qubit's basis and expected outcome: `<{bits}| mv r` is `true` when `r` measures as
`bits`. Expectation values take only fixed labels for now.

**Ordering:** labels map to array indices left to right. `|01>` gives `r` with `r[0]`
in \|0⟩ and `r[1]` in \|1⟩, the same order as the array literal `[|0>, |1>]`. This is
reading order, not Qiskit's little-endian bit-string order; the backends translate.

A ket literal is the only way to create qubits. Because its state is known exactly, the
compiler can track it, which later allows qubits that are provably back in \|0⟩ to be
released automatically (see [ownership.md](ownership.md) §9).

The array repeat form `[|0>; N]` evaluates the ket `N` times, giving `N` independent fresh
qubits. For any other linear value, `[x; N]` is an error, since it would copy `x`.

### Measurement tests

`<s| x` measures `x` and has type `Bool`. `x` must be a `Qubit` when `s` has one label, or
`[Qubit; N]` when `s` has `N` labels. Each label picks the basis for its qubit, as in the
table above: `0` and `1` measure in the Z basis, `+` and `-` in the X basis, `(+i)` and
`(-i)` in the Y basis. The result is `true` when every qubit's outcome matches its label.
`x` is consumed, as with `measure`. Labels can be computed, as in kets.

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

`<ψ| H |ψ> within ε` says how precise an estimate must be: its standard error is at most `ε`,
a positive `Float`. Without `within`, `ε` is 0.01. The precision says what is wanted, not how:
the runtime chooses the runs. When `H = Σ cᵢ Pᵢ`, terms whose Pauli products agree on every
qubit's basis are measured together, and a group whose coefficients sum to `S_g` (in absolute
value) gets `S_g · S / ε²` runs, where `S` sums them over all groups; then the variances add up
to at most `ε²`. An exact value is within any precision, so a simulator ignores `ε` when it
computes the value exactly.

### How the two forms relate

Mathematically, a measurement test is a special case of an expectation value:
|⟨s|ψ⟩|² = ⟨ψ| (|s⟩⟨s|) |ψ⟩, the expectation of the projector onto \|s⟩. The forms differ in
what a quantum computer can physically return:

| Form          | Acts on                               | Returns                                   |
| ------------- | ------------------------------------- | ----------------------------------------- |
| `<s\| mv q`   | a qubit that exists now, consuming it | one sample of \|s⟩⟨s\|: a random `Bool`   |
| `<ψ\| H \|ψ>` | a preparation, re-run as needed      | the average of `H`: a `Float`              |

So `<11| mv r` draws once from the distribution whose probability of `true` is
`<ψ| P(0, 1) * P(1, 1) |ψ>`.

### Amplitudes and transition values

With different preparations on the two sides, the forms mean what they do in physics, and
give a `Complex`:

| Form          | Value       |
| ------------- | ----------- |
| `<φ\|ψ>`      | ⟨φ\|ψ⟩, the amplitude of ψ along φ |
| `<φ\| H \|ψ>` | ⟨φ\|H\|ψ⟩, a transition value       |

`φ` and `ψ` are preparations of the same type, a `Qubit` or `[Qubit; N]`: ket literals with
fixed labels, or calls to functions that are reversible apart from creating their qubits, as
for a bra of a preparation ([ownership.md](ownership.md) §12, N023). If φ = U_φ\|0…0⟩ and
ψ = U_ψ\|0…0⟩, the amplitude is ⟨0…0\|U_φ†U_ψ\|0…0⟩, so it depends on the gates the
preparations apply, global phases included, as Noev's gates define them
([core.md](core.md)). A simulator computes it exactly. Elsewhere it is estimated with
Hadamard tests: one circuit for the real part and one for the imaginary part, each running
the preparations controlled by an extra qubit, and `within ε` bounds each part's standard
error. A transition value sums its terms' amplitudes ⟨φ\|Pᵢ\|ψ⟩, with each term's runs in
proportion to its coefficient, as for an expectation value. When both sides are the same
preparation, `<ψ| H |ψ>` is the expectation value above, a `Float`.

## 4. Conversions

There is no cast operator. Conversions come in three forms, so the call site always shows
what can happen:

| Kind      | Syntax                        | Examples                                      |
| --------- | ----------------------------- | --------------------------------------------- |
| Lossless  | The target type, called       | `Float(n)`, `Bit(flag)`, `Bool(b)`, `Int(b)`  |
| Lossy     | A method named for its effect | `f.trunc()`, `f.round()`, `f.floor()`, `f.ceil()` |
| Can fail  | `T::try_from(x)`              | `u8::try_from(n)`, `i32::try_from(x)`: a `Result<T, String>` |

`Float(n)` is shorthand for `Float::from(n)`. Once traits exist, it is the `From` trait, and
users can define conversions for their own types.

| Conversion                 | Form              | Rule                                  |
| -------------------------- | ----------------- | ------------------------------------- |
| `Int → Float`              | `Float(n)`        | Exact up to 2⁵³, rounded to nearest beyond, infinite past the `Float` range (accepted as lossless by convention) |
| `Float → Int`              | `f.trunc()`, `f.round()`, `f.floor()`, `f.ceil()` | Exact; NaN and infinities are runtime errors |
| `Bit ↔ Bool`               | `Bool(b)`, `Bit(flag)` | `1` ↔ `true`                     |
| `Bit → Int`, `Bool → Int`  | `Int(b)`          | `0` or `1`                            |
| `Int → Bit`                | none in v0.1      | Write `if n == 0 { 0 } else { 1 }`, or wait for `Bit::try_from` |
| Sized integer → `Int`, or to a sized integer that holds every value | `Int(x)`, `i64(x)` | Exact (`u8 → i16`, `i32 → i64`, `u32 → u64`) |
| Any other integer → sized integer | `u8::try_from(n)` | `Ok` when it is in range, otherwise `Err` saying why |
| Integer → `Float`, `f32`   | `Float(n)`, `f32(n)` | As `Int → Float`: rounded past the type's exact range |
| `f32 ↔ Float`              | `Float(x)`, `f32(x)` | Exact widening; narrowing rounds to nearest (accepted by convention) |
| Number → `Complex`, `c32`  | `Complex(x)`, `c32(x)` | The real part, rounded as for the float type; `c32 ↔ Complex` as `f32 ↔ Float` |
| Complex → real             | `z.re()`, `z.abs()` | Say which real number: there is no `Float(z)` |
| `[Bit; N]` ↔ `uN` (N = 8, 16, 32, 64) | `u8(bits)`, `x.bits()` | Exact; `bits[0]` is the most significant, so a bit string reads as the number it prints as: `u8(\|00000101>.measure()) == 5` |

There are no conversions to or from `Qubit`. The only way from quantum to classical is
measurement.

## 5. Linearity

Every type is **copyable**, **affine** or **linear**:

- `Int`, `Float`, the sized numbers, the complex numbers, `Bool`, `Bit`, `String`, `Observable`,
  `Distribution<T>`, `()` and shared references `ref T` are copyable.
- Exclusive references `mut T` are affine: they cannot be copied, but can be dropped.
- `Qubit` is linear: it cannot be copied or dropped, and must be consumed.
- A tuple, array or struct is linear if any component is linear; otherwise affine if any
  component is affine; otherwise copyable. An enum is the same, over the data of all its
  variants: `Option<Qubit>` is linear, `Option<Int>` copyable.

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
| enums                        | `Basis::X`, `Outcome::Success(1)`, `Some(3)`, `None`   |
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
