# Grammar

Noev has its own syntax. It keeps Rust's ownership model and much of its feel (`fn`, `->`,
`struct`, `::` paths, braces), but differs wherever there is a reason to: ownership is
written with three matching words (`mv`, `ref`, `mut`), methods live inside the struct,
logic uses `and`/`or`/`not`, conversions look like constructors, and semicolons are
optional. Quantum features are introduced by contextual keywords, so they never take names
away from classical code.

## 1. Lexical structure

Source files are UTF-8 with the extension `.nv`.

### Comments and whitespace

`// line comment` and `/* block comment */`. Block comments nest. Spaces and tabs only
separate tokens. Line breaks can end statements (§1, Line breaks).

**Doc comments.** Lines starting with `///` directly above a function, method, struct or
field document it. They are ordinary comments to the compiler; tools such as the language
server show them.

```noev
/// The receiver's corrections, from the sender's two measurements.
fn correct(receiver: mut Qubit, m1: Bit, m2: Bit) { … }
```

### Identifiers

`[A-Za-z_][A-Za-z0-9_]*`, excluding reserved keywords. `_` on its own is a wildcard
pattern, not an identifier.

### Keywords

Reserved keywords cannot be used as identifiers:

```
and  as  assert  break  continue  else  enum  extern  false  fn  for  if  impl  in  let
match  mut  not  or  pub  ref  return  sample  self  Self  struct  trait  true  type  unsafe
use  while
```

Contextual keywords are keywords only in the positions listed. Everywhere else they are
ordinary identifiers, so `let adj = build_graph(edges)` is valid.

| Keyword | Is a keyword when…                                          |
| ------- | ----------------------------------------------------------- |
| `mv`    | it is followed by a place expression (`mv q`, `mv e.state`) or by `self` in a parameter list |
| `rev`   | it is directly followed by `fn` or `circuit`                |
| `circuit` | it is directly followed by `fn` (see [execution.md](execution.md) §4) |
| `adj`   | it is followed by a call (`adj s(q)`, `adj q.s()`)          |
| `ctrl`  | it starts a statement and is followed by a place and, eventually, `{` |
| `with`  | it starts a statement and is directly followed by `{`       |
| `do`    | it directly follows the closing `}` of a `with` block       |
| `within` | it directly follows an expectation value (see below)       |
| `pkg`, `super` | they start a path and are followed by `::` (Reserved; see [modules.md](modules.md)) |

### Literals

| Literal  | Examples                             | Notes                              |
| -------- | ------------------------------------ | ---------------------------------- |
| Integer  | `0`, `42`, `1_000`, `0xff`, `0b1010` |                                    |
| Float    | `1.0`, `3.14`, `1e-3`, `2.5e6`       | A `.` must be followed by a digit  |
| Imaginary | `2.0i`, `3i`, `1e-3i`               | A number with `i` written directly after it |
| Bool     | `true`, `false`                      |                                    |
| Ket      | `\|0>`, `\|1>`, `\|+>`, `\|->`, `\|01>`, `\|+0->` | See below             |
| Bra      | `<0\|`, `<1\|`, `<+\|`, `<-\|`, `<01\|`           | See below             |
| String   | `"text"`, `"θ = {theta:.3}"`, `"a\tb\n"`           | See below             |

There is no separate `Bit` literal. The integer literals `0` and `1` have type `Bit` where
the context requires it (see [types.md](types.md)).

### Ket literals

A ket literal is `|`, then one or more labels, then `>`, with no whitespace inside. A label is
one of the characters `0 1 + -`, `(+i)` or `(-i)`, or a **computed label** `{expr}`, an
expression in braces as in string interpolation. Each fixed label is one qubit; a computed
one is as many as its value holds ([types.md](types.md) §3). It creates freshly prepared qubits (see [types.md](types.md)).

`|` is also the bitwise-or operator. The two never conflict, because a ket is only
recognised **where an operand can start**. The lexer treats `|` as the start of a ket unless
the previous token, on the same line, ends an operand: an identifier, a literal, a ket,
`self`, `)` or `]`. At the start of a line, `|` begins a ket if it forms one.

```noev
let q = |0>         // operand position: ket
let m = a | 0       // after an identifier: bitwise or
let n = (a) |0> b   // after `)`: `|`, `0`, `>`, so bitwise or then comparison
```

Inside a ket, `+-` does not lex as `+` and `-` (or `->`), nor `(+i)` as a parenthesised
expression; the ket is one token.

### Bra literals

A bra literal is `<`, then one or more labels (as in a ket), then `|`, with no whitespace
inside. Like a ket, it is only recognised where an operand can start, so
`a < 0|b` is still a comparison followed by bitwise or. Bras are used for measurement tests
and expectation values (§2, Bras and expectation values).

### String literals

A string literal is text between double quotes. It may span lines. Escapes:

| Escape            | Meaning                       |
| ----------------- | ----------------------------- |
| `\n`, `\t`, `\r`  | Line feed, tab, carriage return |
| `\\`, `\"`        | Backslash, double quote       |
| `\0`              | The null character            |
| `\u{1F600}`       | The Unicode scalar value with that hexadecimal code |
| `{{`               | A literal `{`                 |

Only `{` is special in a string: it starts an interpolation, and `{{` writes a literal `{`.
A `}` outside an interpolation is always just a `}`.

**Interpolation.** `{expr}` inside a string inserts the value of `expr`, and `{expr:spec}`
formats it:

```noev
let s = "θ = {theta:.4}, outcome {bits}, {n:>5} shots, mask {m:08b}"
```

The expression is any expression without a top-level `:` or unbalanced braces; it may contain
string literals of its own. Interpolation is checked at compile time: the expressions are
type-checked, each format spec is checked against its value's type
([types.md](types.md) §7), and nothing is parsed at runtime.

### Operators and punctuation

```
+  -  *  /  %  &  |  ^  ~  <<  >>
== != <  <=  >  >=  and  or  not
=  += -= *= /= %= &= |= ^= <<= >>=
.  ,  ;  :  ::  ->  =>  ..  (  )  [  ]  {  }
```

`and`, `or` and `not` are logical operators on `Bool` (`and`/`or` short-circuit). `&`, `|`,
`^` and `~` are bitwise operators on `Int` and `Bit`. `!` is not an operator; it is reserved
(see Open questions).

### Line breaks

Semicolons are optional. A line break ends a statement when all of these hold:

1. the innermost enclosing bracket is not `(` or `[` (line breaks directly inside those are
   ignored; inside a `{ … }` nested within them, they count again);
2. the last token on the line can end an expression: an identifier, a literal, a ket,
   `self`, `Self`, `)`, `]`, `}`, `break`, `continue`, `return`, or `>` or `->` (which can
   close an expectation value's ket);
3. the next line does not start with a token that can only continue an expression: `.`,
   a binary operator other than `-` and `<`, `else`, or `do`.

Where an operand must follow (after a comparison operator, or after `->` in a function
signature), a line break is skipped, so `a >` at the end of a line continues onto the next.

A line starting with `-`, `<` or `(` begins a new statement (`-x`, `<ψ| H |ψ>` and `(a, b)`
can all start one). To continue an expression across
lines, end the line with the operator or wrap it in parentheses. A `;` can always be written,
and separates statements on one line.

```noev
let total = first
    + second            // continues: a line can't start with `+`
q.h()
    .s()                // continues: a line can't start with `.`
let x = a
-b                      // new statement: `-b`
```

## 2. Syntax

The EBNF below uses `{ x }` for zero or more, `[ x ]` for optional, and `x | y` for choice.
Terminals are quoted. `sep` is a line break (as defined above), `;` or `,` where shown.

### Items

```ebnf
program     = { item } ;
item        = struct_def | enum_def | type_alias | fn_def | trait_def | impl_def | extern_def ;

type_alias  = "type" IDENT "=" type ;

struct_def  = "struct" IDENT "{" { member ( sep | "," ) } "}" ;
member      = field | fn_def ;
field       = IDENT ":" type ;

enum_def    = "enum" IDENT "{" { variant ( sep | "," ) } { fn_def sep } "}" ;
variant     = IDENT
            | IDENT "(" type { "," type } [ "," ] ")"            (* values *)
            | IDENT "{" field { ( sep | "," ) field } [ "," ] "}" ;  (* named fields *)

fn_def      = [ "unsafe" ] [ "rev" ] [ "circuit" ] "fn" IDENT "(" [ params ] ")" [ "->" type ] block ;
params      = ( receiver | param ) { "," param } [ "," ] ;
receiver    = ( "mv" | "ref" | "mut" ) "self" ;
param       = IDENT ":" type ;

trait_def   = "trait" IDENT "{" { trait_item sep } "}" ;              (* Reserved *)
trait_item  = fn_sig | fn_def | "type" IDENT ;
fn_sig      = [ "unsafe" ] [ "rev" ] [ "circuit" ] "fn" IDENT "(" [ params ] ")" [ "->" type ] ;
impl_def    = "impl" path "for" type "{" { impl_item sep } "}" ;     (* Reserved *)
impl_item   = fn_def | "type" IDENT "=" type ;
extern_def  = "extern" fn_sig | "extern" STRING "{" { fn_sig sep } "}" ;   (* Reserved *)
```

A struct holds its fields and its methods together:

```noev
struct Experiment {
    angle: Float
    state: Qubit

    fn new(angle: Float) -> Self { … }          // associated function: Experiment::new(…)
    fn rotate(mut self) -> mut Self { … }       // method: e.rotate()
}
```

Functions inside a struct with a receiver are **methods**, called as `x.name(…)`. Those
without one are **associated functions**, called as `Type::name(…)`.

The receiver states how the method takes the value, using the same words as call sites:
`mv self` takes ownership, `ref self` borrows to read, `mut self` borrows to change.

`extern` declares host and C functions; see [interop.md](interop.md). Traits and
`impl … for …` blocks are Reserved: their syntax is specified so that it can be
parsed, but they need generics, which come after v0.1. See [types.md](types.md) §6.

### Types

```ebnf
type        = path [ "<" type { "," type } [ "," ] ">" ]   (* Int, Float, Bool, Bit, Qubit, String, Self, struct name, Distribution<T> *)
            | "ref" type                          (* shared reference *)
            | "mut" type                          (* exclusive reference *)
            | "(" ")"                             (* unit *)
            | "(" type "," [ type { "," type } [ "," ] ] ")"   (* tuple *)
            | "[" type ";" INT "]" ;              (* array of fixed length *)
```

### Statements and blocks

```ebnf
block       = "{" { stmt ( sep | ";" ) } [ expr ] "}" ;

stmt        = "let" [ "mut" ] pattern [ ":" type ] "=" expr
            | place assign_op expr
            | "return" [ expr ]
            | "break" | "continue"
            | "assert" expr
            | expr ;

pattern     = IDENT | "_"
            | "(" pattern { "," pattern } [ "," ] ")"      (* a tuple *)
            | "[" pattern { "," pattern } [ "," ] "]"      (* an array, one pattern per element *)
            | path [ "(" pattern { "," pattern } [ "," ] ")"                 (* a variant *)
                   | "{" field_pattern { "," field_pattern } [ "," ] [ ".." ] "}" ]
            | literal_pattern ;
field_pattern = IDENT [ ":" pattern ] ;              (* `attempts` is short for `attempts: attempts` *)
literal_pattern = [ "-" ] INT | "true" | "false" | STRING ;
assign_op   = "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "&=" | "|=" | "^=" | "<<=" | ">>=" ;
```

A block's value is its last statement, if that is an expression not followed by `;`.
Otherwise the block's value is `()`. In a block whose type is `()` (a function with no
`->`, a loop body, a `ctrl` body), the value of the last expression is discarded; discarding
a linear value is an error (N003). So `ctrl a { b.x() }` is fine even though `b.x()` returns
a reference.

`let` declares a variable that cannot be reassigned. `let mut` allows reassignment and
changes to its classical data. Quantum state can always be changed by gates, whichever is
used (see [ownership.md](ownership.md) §2).

### Control flow

```ebnf
if_expr     = "if" expr_no_struct block [ "else" ( block | if_expr ) ] ;
for_expr    = "for" IDENT "in" expr_no_struct ".." expr_no_struct block ;
while_expr  = "while" expr_no_struct block ;
sample_expr = "sample" expr_no_struct block ;
match_expr  = "match" expr_no_struct "{" { arm ( "," | sep ) } "}" ;
arm         = pattern "=>" expr ;

ctrl_stmt   = "ctrl" place { "," place } block ;
with_stmt   = "with" block "do" block ;          (* Reserved *)
unsafe_block = "unsafe" block ;
```

`expr_no_struct` is an expression that cannot be a struct literal at its top level, so that
`if x { … }` parses the block as the body.

- `if` takes a classical condition (`Bool` or `Bit`) and branches at runtime.
- `ctrl` takes qubits (or pure quantum values, whose qubits all act as controls) and applies
  its body under quantum control. Using a qubit as an `if`
  condition, or a classical value as a `ctrl` control, is a type error.
- `for i in a..b` iterates over the half-open integer range.
- `assert cond` checks a `Bool` condition at runtime and stops the program if it is false.
  The compiler also uses it as a known fact (see [ownership.md](ownership.md) §10).
- `sample n { … }` runs its block `n` times, each run separate from the others and from the
  code around it, and counts the results (see [execution.md](execution.md) §3).
- `match x { p => a, q => b }` runs the first arm whose pattern fits `x`, and its value is
  that arm's. The arms must cover every value of `x`'s type, looking inside tuples, arrays
  and variants' data: every variant of an enum, `true` and `false`, `0` and `1`, every
  combination of them in a tuple (`(0, 0)`, `(0, 1)`, `(1, _)`), or a catch-all (`_` or a
  name) for numbers and strings. An error names a value no arm takes, such as `(1, 1)`.
  Patterns that only test (`_`, literals, variants without data) can be used in any
  `match`; patterns that bind take parts of the value, by the rules of
  [ownership.md](ownership.md) §13.
- A plain name in a pattern binds a variable, except `None`, `Some`, `Ok` and `Err`, which
  are the variants of `Option` and `Result`, in patterns and expressions alike: no variable
  or function can take those names. Other variants are always written with their enum's
  name (`Basis::X`), so a pattern never looks like a variable by accident.
- `unsafe { … }` allows operations whose claims the compiler cannot check (see
  [ownership.md](ownership.md) §11).

### Expressions

```ebnf
expr        = or_expr ;
or_expr     = and_expr { "or" and_expr } ;
and_expr    = not_expr { "and" not_expr } ;
not_expr    = "not" not_expr | binary ;
binary      = unary { BINOP unary } ;              (* precedence below *)
unary       = { "-" | "~" } postfix
            | ( "mv" | "ref" | "mut" ) place { method_call }
            | "adj" postfix                         (* must end in a single call *)
            | BRA unary ;                           (* measurement test *)
postfix     = primary { "." IDENT | "." INT | "[" expr "]" | method_call | "(" args ")" } ;
method_call = "." IDENT "(" args ")" ;
args        = [ expr { "," expr } [ "," ] ] ;

primary     = INT | FLOAT | "true" | "false" | KET
            | path
            | path "{" [ field_init { ( "," | sep ) field_init } [ "," ] ] "}"   (* struct literal *)
            | "(" expr ")"
            | "(" ")"
            | "(" expr "," [ expr { "," expr } [ "," ] ] ")"           (* tuple *)
            | "[" [ expr { "," expr } [ "," ] ] "]"                    (* array *)
            | "[" expr ";" expr "]"                                     (* array repeat *)
            | expectation
            | if_expr | for_expr | while_expr | sample_expr | match_expr | ctrl_stmt
            | with_stmt | unsafe_block | block ;
expectation = ( "<" prep "|" observable "|" prep ">"
              | BRA observable "|" KET_BODY ">" )   (* <+| h |+> *)
              [ "within" unary ] ;                  (* the precision *)
prep        = postfix ;                             (* a call or a ket *)
observable  = binary ;                              (* without `|` or comparisons at top level *)
path        = ( IDENT | "self" | "Self" ) { "::" IDENT } ;
field_init  = IDENT [ ":" expr ] ;                  (* `angle` is short for `angle: angle` *)

place       = ( IDENT | "self" ) { "." IDENT | "." INT | "[" expr "]" } ;
```

`.0`, `.1`, … index tuples. `::` is for types and namespaces (`Experiment::new`,
`Float::PI`); `.` is for values (fields and methods).

### Ownership words

Three prefix words say what happens to a place:

| Word        | Meaning                    | Type it produces |
| ----------- | -------------------------- | ---------------- |
| `mv place`  | give the value away        | `T`              |
| `ref place` | lend it to be read         | `ref T`          |
| `mut place` | lend it to be changed      | `mut T`          |

`ref` and `mut` can usually be left out; `mv` never can (see
[ownership.md](ownership.md) §3–4).

A variable holding a reference is used as if it were the value it refers to: `n += 1` with
`n: mut Int` changes the caller's integer. There is no dereference operator. To lend a
reference on, use the same words: `mut n`.

### Method calls and chaining

`x.f(a)` calls the method `f` of `x`'s type. The receiver is lent automatically according to
the method's receiver (`ref self` or `mut self`); a `mv self` receiver needs `mv` in front.

Chaining follows return types. Gates take `mut self` and return `mut Self`, so each call in a
chain applies to the same qubit:

```noev
q.h().s().ry(theta)
e.state.x().rz(0.5)
let r = q.h()             // r: mut Qubit, lending q
r.x()                     // q stays lent while r is in use
let s = angle.abs().sqrt()
```

`mv` covers the longest place before the first method call: `mv e.state.measure()` moves
`e.state` into `measure`. A chain cannot end in a consuming call on a lent qubit
(`q.h().measure()` would move out of `mut Qubit`, error N007), so consume separately:

```noev
q.h().s()
let b = mv q.measure()
```

Every gate is also a free function, for circuit-style code: `h(q)`, `ry(q, theta)`,
`swap(a, b)`, `measure(mv q)`. Functions acting on qubits take the qubits first and
classical parameters after.

### Bras and expectation values

A bra followed by a qubit value is a **measurement test**: `<s| mv q` measures `q` in the
basis containing `s`, consumes it, and is `true` if the outcome was `s` (see
[types.md](types.md) §3).

```noev
let hit = <1| mv q          // Z basis
if <+| mv a { … }           // X basis
let all = <01| mv r         // r: [Qubit; 2], true when r[0] is 0 and r[1] is 1
```

A bra, an observable and a ket together are an **expectation value** `<ψ| H |ψ>`, written as
in physics:

```noev
let energy = <ansatz(theta)| h |ansatz(theta)>
let zero = <+| Z(0) |+>
```

`ψ` is a **preparation**: a function call or a ket literal, which the runtime may evaluate
many times. With the same expression on both sides it is an expectation value; with
different ones, a transition value (below). The observable is an expression whose top level contains no `|` and no
comparison, so the `|` and `>` that close it are unambiguous. Inside the closing ket, `|`
always starts the preparation, even after an operand.

To tell the two forms apart, the parser reads an observable-shaped expression after the
bra. If `|` follows, the expression is an expectation value. Otherwise it is a measurement
test, and the expression must be a single operand (`<1| mv q`), so `<1| mv q and ok` means
`(<1| mv q) and ok`. A `<` that does not form a bra literal, in operand position, starts a
bra holding a preparation, which is told apart the same way: `<bell()| Z(0) |bell()>` is an
expectation value, and `<bell()| mv pair` a measurement test against `bell()`'s state
([ownership.md](ownership.md) §12). A bra followed directly by a ket, `<φ|ψ>`, is an
amplitude, and different preparations on the two sides of `<φ| H |ψ>` make a transition
value ([types.md](types.md) §3); `ψ` may be a call or labels (`<bell()|00>`, `<0|+>`).
Since an expectation value or amplitude ends with `>`, a method call may follow it directly:
`<ψ| H |ψ>.round()`, `<φ|ψ>.abs()`.

`within ε` after an expectation value asks for a standard error of at most `ε` when the value
is estimated ([types.md](types.md) §3). `ε` is a unary expression, so
`<ψ| H |ψ> within 0.01 * 2.0` doubles the value; write `within (0.01 * 2.0)` for a computed
precision.

### Conversions

Converting between types looks like calling the target type: `Float(n)`, `Bit(true)`,
`Int(b)`. This is shorthand for `Float::from(n)` and only exists for conversions that keep
the value (see [types.md](types.md) §4). Conversions that lose information are methods named
for what they do: `f.trunc()`, `f.round()`. There is no `as`.

### Operator precedence

Highest first. Binary operators are left-associative; comparisons cannot be chained.

| Level | Operators                                        |
| ----- | ------------------------------------------------ |
| 1     | `.` field and method, `[]` index, `()` call, `::` path |
| 2     | unary `-` `~`, `mv`, `ref`, `mut`, `adj`, bra test |
| 3     | `*` `/` `%`                                      |
| 4     | `+` `-`                                          |
| 5     | `<<` `>>`                                        |
| 6     | `&`                                              |
| 7     | `^`                                              |
| 8     | `\|`                                             |
| 9     | `==` `!=` `<` `<=` `>` `>=`                      |
| 10    | `not`                                            |
| 11    | `and`                                            |
| 12    | `or`                                             |

As in Python, `not a == b` means `not (a == b)`.

## 3. Examples

```noev
fn bell_pair() -> (Bit, Bit) {
    let a = |+>
    let b = |0>
    ctrl a { b.x() }
    (mv a.measure(), mv b.measure())
}

struct Experiment {
    angle: Float
    state: Qubit

    fn new(angle: Float) -> Self {
        Experiment { angle, state: |0> }
    }

    fn rotate(mut self) -> mut Self {
        self.state.ry(self.angle)
        mut self
    }

    fn finish(mv self) -> Bit {
        mv self.state.measure()      // `angle` is copyable, so `self` is fully used up
    }
}

fn run(angle: Float) -> Bit {
    let mut e = Experiment::new(angle)   // `let mut`: e has a classical field
    e.rotate().rotate()
    mv e.finish()
}

fn majority(a: mut Qubit, b: mut Qubit, c: mut Qubit) {     // inferred as reversible
    ctrl c { b.x(); a.x() }
    ctrl a, b { c.x() }
}

fn ghz() -> [Bit; 4] {
    let r = |0000>
    r[0].h()
    for i in 0..3 {
        ctrl r[i] { r[i + 1].x() }
    }
    mv r.measure()
}

fn bump(n: mut Int) {
    n += 1
}

fn main() {
    let mut count = 0
    bump(mut count)                  // classical change: written out
    if count > 0 and not count == 3 {
        let f = Float(count) * Float::PI
    }
    let (x, y) = bell_pair()
}
```

## 4. Open questions

- **Explicit type arguments in expressions.** Type arguments are written with angle
  brackets, `Distribution<(Bit, Bit)>`, which is unambiguous in type positions. Writing them
  inside an expression (calling a generic function with explicit types) would clash with
  `<`; Rust's `f::<T>()` is the likely answer, decided with generics.
- **Negative controls.** `ctrl !a { … }` or `ctrl not a { … }` (control on \|0⟩). `!` is
  reserved for this or similar. Not in v0.1.
- **`adj` on blocks.** `adj { … }` could invert a whole block. v0.1 only allows `adj` on
  single calls.
- **Closures.** If added, the `|x| …` form can coexist with kets, because a ket contains
  only `0 1 + -` and ends in `>`. Not needed for v0.1.
- **Modules and imports.** Specified in [modules.md](modules.md); Reserved. In v0.1 one
  `.nv` file is one program, and `main` is the entry point when one exists.
