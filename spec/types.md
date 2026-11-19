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
the implementation makes cheap by sharing the text. Strings are immutable for now and can be
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
  [ownership.md](ownership.md) §4–5. Only elided lifetimes are supported so far; references stored
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

The value is ⟨ψ|H|ψ⟩. A backend that can compute it exactly, such as a simulator
([runtime.md](runtime.md) §3), does so when `ψ` contains no measurement; otherwise it is
estimated by averaging over repeated runs of `ψ`.

`<ψ| H |ψ> within ε` says how precise an estimate must be: its standard error is at most `ε`,
a positive `Float`. Without `within`, `ε` is 0.01. The precision says what is wanted, not how:
the runtime chooses the runs. When `H = Σ cᵢ Pᵢ`, terms whose Pauli products agree on every
qubit's basis are measured together, and a group whose coefficients sum to `S_g` (in absolute
value) gets `S_g · S / ε²` runs, where `S` sums them over all groups; then the variances add up
to at most `ε²`. An exact value is within any precision, so a backend ignores `ε` when it
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
([core.md](core.md)). A backend that can, such as a simulator, computes it exactly.
Elsewhere it is estimated with
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
| `Int → Bit`                | none yet          | Write `if n == 0 { 0 } else { 1 }`, or wait for `Bit::try_from` |
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

## 6. Traits and generics

Status: **Implemented**: type parameters of functions, structs and enums, bounds (`Copy` and
traits) written or inferred, inference of type arguments, `::<…>`, traits with and without
type and size parameters, operator traits (`std::ops`), sums of sizes (`N + 1`), associated types (with `Copy` as their only bound), `impl`s (generic ones too) and monomorphisation, `Trait::f(x)`, `Measure`
(for qubits, and tuples and arrays of measurable types), and size parameters. The syntax is
in [grammar.md](grammar.md) §2.

### Generic parameters

Functions, structs, enums and traits can take **type parameters** and **size parameters**:

```noev
fn swap<T>(pair: (T, T)) -> (T, T) {
    let (a, b) = mv pair
    (mv b, mv a)
}

fn qft<const N: Int>(r: mut [Qubit; N]) { … }      // a register of any size

struct Pair<T, U> {
    first: T
    second: U
}
```

- A size parameter, `const N: Int`, is a non-negative `Int` known when the program is
  compiled, so `[T; N]` is an array of fixed length. In the body, `N` is also an `Int`
  value (`for i in 0..N`), and `[|0>; N]` makes an array of that length. Indices into a
  qubit array of length `N` are proven against `N` ([ownership.md](ownership.md) §10), and
  a size parameter is never negative.
- A size can be computed from size parameters and numbers with `+`, and `*` with a number
  on one side: `[Qubit; N + 1]`, `[Bit; 2 * N]`, `[Int; N + M]`. As a type argument it goes in
  braces, so its `+` is not read as part of a type: `Reg<{N + 1}>`. Sizes are compared after
  normalising, so `1 + N` and `N + 1` are the same, as are `N + N` and `2 * N`. Subtraction
  is not allowed yet, since `N - 1` could be negative: take the larger size as the
  parameter.
- A call infers a size parameter from a sum when it is the only one unknown: given
  `[Qubit; 5]` for `[Qubit; N + 1]`, `N` is 4; given `[Bit; 6]` for `[Bit; 2 * N]`, `N` is 3.
  When none fits (`[Bit; 5]` for `2 * N`), the types do not match.
- Type and size arguments are inferred where the function is called, from its arguments
  and then from the type expected of the call (`let r: [Qubit; 4] = fresh()`), or given
  after `::`: `swap::<Int>((1, 2))`, `qft::<8>(mut r)`. A struct's are given or inferred
  the same way: `Reg<4>`, or `Reg { qs: |0110> }`.

### What generic code can do with a value

A type parameter whose body needs nothing of it accepts **any** type, classical or quantum:
`swap((1, 2))` and `swap((|0>, |1>))` both work. Such a body treats a value of type `T` as it
would a qubit: it moves it with `mv` and consumes it. For a classical value, that costs
nothing. A body that copies or drops a `T` needs `T: Copy`, written or inferred (below).

**`T: Copy`** is the one bound about values themselves: `T` is copyable (§5), so the body can
copy and drop its values:

```noev
fn twice<T: Copy>(x: T) -> (T, T) { (x, x) }
```

No function can copy "a classical value or a qubit", since an unknown quantum state cannot be
copied, so the bound marks exactly what a body cannot do for qubits. `Copy` is decided by the
compiler from a type's structure (§5), never written in an `impl`: classical values are
`Copy`, and so is a shared reference `ref T`, even to a qubit. An exclusive reference `mut T`
can be dropped but not copied, so it is not `Copy`; given as a `T`, it is treated like a
qubit: passed on or returned, never dropped. There is no bound for "can be dropped but not
copied" yet; one can be added without breaking anything.

There is no bound saying a type holds qubits: it would let a body do nothing new. What a
body does with a quantum value is a trait of its own, such as `Measure`.

### Bounds

A bound is any trait, the standard library's or the program's: `T: Measure`, `T: Oracle`.
Several are joined with `+` (`T: Copy + Add`), and long ones move to `where`:

```noev
fn zip<T, U, const N: Int>(a: [T; N], b: [U; N]) -> [(T, U); N]
where
    T: Copy,
    U: Copy,
{ … }
```

A bound can fix a trait's associated type: `T: Measure<Result = Bit>` is something that
measures to one bit.

### Bounds are inferred within a package

As with reversibility ([modules.md](modules.md) §5), the compiler works out what a generic
function needs from its body, within its package: a body that copies or drops a `T`, or
otherwise uses one as only a copyable value can be (moving it on every turn of a loop, or in
reversible code), needs `T: Copy`, and one that calls `x.measure()` on a `T` needs
`T: Measure`. A call that
gives a type without what the function needs is an error at the call, naming the line that
needs it:

```noev
fn twice<T>(x: T) -> (T, T) { (x, x) }     // needs `T: Copy`

twice(1)                                    // fine
twice(|0>)      // error: `twice` needs `T: Copy`, but here `T` is `Qubit`
                //   line 1, at the second `x`: `T: Copy` is needed here
```

- What a body needs comes from what it does with its own values, and from the functions it
  calls: passing a `T` to `twice` needs what `twice` needs, so needs follow calls through
  any number of functions, recursion included.
- A method called on a `T` (`x.f()`) needs the one trait with a function `f`; when several
  traits have one, the bound must be written. Traits with parameters are not inferred.
- Writing a bound is always allowed; it turns the inference into a promise, checked in the
  body: a parameter with any bound written, inline or in `where`, has exactly the bounds
  written, so copying a `T: Measure` is N001. A struct's or enum's parameters, and those of
  traits and `impl`s, are always written.
- An associated type in a signature (`T::Result`) needs its trait written as a bound, since
  signatures are read before bodies; so do `Distribution<T>` and `sample` returning a `T`,
  which need `T: Copy` written.

A generic function visible to other packages (`pub` in a library) states its bounds: they
are part of its signature, so a library's update cannot break a program by changing a body,
and a compiled library's signatures are fixed. Its body is checked against the bounds
written, as if each were a promise.

### Traits

```noev
trait Oracle<const N: Int> {
    rev fn apply(ref self, r: mut [Qubit; N])
    fn name(ref self) -> String { "oracle" }        // a default
}

impl Oracle<4> for Parity {
    rev fn apply(ref self, r: mut [Qubit; 4]) { … }
}
```

- A trait declares functions, with or without default bodies, and associated types
  (`type Result`). An `impl` provides them for a type, and can itself be generic:
  `impl<T: Measure> Measure for Pair<T, T>`.
- A trait can take type and size parameters, which its functions see after `Self`. A type
  can implement one trait several times with different arguments (`Oracle<2>` and
  `Oracle<4>`), and a call picks the `impl` by them: they are inferred from the call's
  arguments, as a function's are; if that leaves one open, it is the trait's only arguments
  for the type (from its bounds, for a type parameter); otherwise the call is an error.
- A generic trait's associated types depend on its arguments: `Scale<Int>` and
  `Scale<Float>` can each give `Twice` a different `Out`. `S::Out` takes the arguments of
  `S`'s bound (`S: Scale<Int>`), or of the type's only `impl`s of the trait; when a type
  implements it with several, `Twice::Out` is an error. A bound can fix one as for any
  trait: `S: Scale<Int, Out = Int>`.
- Methods are written inside structs and enums, so `impl` is only for traits.
- **Coherence**, as in Rust: an `impl` must be in the package of the trait or of the type, so
  two packages can never provide the same one.
- A trait function declared `rev fn` is reversible in every `impl`, which is checked there.
  Generic code cannot see an `impl`'s body, so this is how `adj oracle.apply(r)` and
  `ctrl c { oracle.apply(r) }` work on any `T: Oracle<N>`.
- A call on a known type reaches its `impl`'s function, which is analysed like any other:
  its reversibility is inferred within a package, and a call of a function not declared
  `rev fn` on a concrete type can still be part of a circuit.
- When two traits a type implements both have a function `f`, `x.f()` is an error; say
  which with `Trait::f(x)`. `Trait::f(x)` also works when only one has it.
- There are no trait objects (`dyn Trait`): generic code is compiled for each type it is
  used with, so qubit counts are always known.

### `Measure`

Measurement is a trait, pairing a quantum type with its classical result type:

```noev
trait Measure {
    type Result: Copy
    fn measure(mv self) -> Self::Result
}
```

`Qubit` measures to `Bit`, `[T; N]` to `[T::Result; N]` and a tuple to the tuple of its
elements' results. A struct measures by an `impl` of its own, such as `Experiment` to a
classical `ExperimentResult`. The free function `measure(x)` is `measure<T: Measure>(x: T)
-> T::Result`.

### Operator traits

`+`, `-`, `*` and unary `-` on a type of the program's own call the function of a trait in
`std::ops`, which the program implements:

```noev
use std::ops::{Add, Mul}

impl Add<V> for V {
    type Output = V
    fn add(mv self, rhs: V) -> V { V { x: self.x + rhs.x } }
}

impl Mul<Int> for V {                  // `v * 2`
    type Output = V
    fn mul(mv self, k: Int) -> V { V { x: self.x * k } }
}
```

| Operator | Trait         | Function                                   |
| -------- | ------------- | ------------------------------------------ |
| `a + b`  | `Add<Rhs>`    | `fn add(mv self, rhs: Rhs) -> Self::Output` |
| `a - b`  | `Sub<Rhs>`    | `fn sub(mv self, rhs: Rhs) -> Self::Output` |
| `a * b`  | `Mul<Rhs>`    | `fn mul(mv self, rhs: Rhs) -> Self::Output` |
| `-a`     | `Neg`         | `fn neg(mv self) -> Self::Output`           |

- `a + b` is `Add::add(a, b)`: the `impl` is picked by the right side's type, as for any
  generic trait, so `v * 2` and `v * w` can be different `impl`s with different `Output`s.
- Both sides are given by value, so they follow the rules for arguments: a side holding
  qubits is moved with `mv` (`mv a + mv b`, N001 otherwise), and a copyable one is copied.
- In generic code the bound is written: `T: Add<T, Output = T>`. Operators do not infer
  bounds, since their traits have parameters.
- Built-in types keep their built-in operators. They do not implement the `std::ops` traits
  yet, so a `T: Add<T>` cannot be given an `Int`.
- The traits are not in the prelude: `use std::ops::Add` to implement or bound one.

### Built-in generic types

`Option<T>` and `Result<T, E>` are ordinary generic enums in the prelude, with their variants
also usable bare (`Some`, `None`, `Ok`, `Err`). They can hold qubits: an `Option<Qubit>` must be
consumed, with `match mv`. `Distribution<T>` is built in, with `T: Copy`: it holds the
results of many runs.

### Compilation

Generic code is **monomorphised**: compiled once for each combination of type and size
arguments it is used with. Circuits need concrete sizes, a target's checks need exact qubit
counts, and native code needs concrete types. Types and ownership are checked once, on the
generic body; circuits are found, and code is generated, for each copy. (For now circuits are
found once, on the generic body, and a run that calls a trait's function not declared
`rev fn` through a bound runs shot by shot, since what its `impl`s do is not looked at
there; a `rev` one cannot measure, so it keeps a run a circuit.)

### Not yet

Subtraction and other arithmetic in sizes beyond `+` and `*` by a number; `From` beyond the
built-in conversions; `impl Trait` arguments;
closures; the quantum interfaces `Oracle`, `Ansatz` and `Observable` in the standard library.
The `std::ops` traits for built-in types, defaults for trait parameters (`Add<Rhs = Self>`),
and traits for `/`, `%`, comparisons and the bit operators; `AddAssign` and the other
compound-assignment traits, so `a += b` changes a value of the program's own in place.

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

- **Classical values that are expensive to copy** (growable arrays, maps): `String`
  and `Distribution` use value semantics with shared storage; whether growable collections do
  the same, as in Swift, is open.
- **String operations**: length, concatenation, slicing, and how indexing relates to Unicode.
- **Generic parameter syntax**: see [grammar.md](grammar.md) §4.
- **Angles.** Whether rotation angles get their own type (with units) or stay `Float`.
