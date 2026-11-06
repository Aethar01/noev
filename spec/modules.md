# Modules and packages

Status: **Reserved.** v0.1 compiles one file at a time. The design is fixed now because it
shapes name resolution, visibility, and the rules for reversibility across modules. The
keywords are reserved already, so no program can start using them as names.

## 1. Files are modules

There are no module declarations: the file tree is the module tree.

- A **package** is a directory with a `noev.toml` manifest. It is the unit of distribution
  and of dependency.
- Each `.nv` file in the package's `src/` directory is a **module**, named by its path:
  `src/circuits/ladder.nv` is the module `circuits::ladder`.
- A directory is a module too. Items of the module `circuits` itself go in `src/circuits.nv`,
  beside the directory, if there are any.
- The package's root module is `src/main.nv` (a program, with `fn main`) or `src/lib.nv` (a
  library).

```
vqe/
├── noev.toml
└── src/
    ├── main.nv              // the root module
    ├── ansatz.nv            // ansatz
    ├── hamiltonians.nv      // hamiltonians
    └── hamiltonians/
        └── ising.nv         // hamiltonians::ising
```

A single file still works on its own: `noev run file.nv` treats it as a package with one
module, so every v0.1 program stays valid.

Rust's `mod` declarations were left out deliberately: they repeat what the file tree
already says.

## 2. Paths

Items are named by paths, with `::` as everywhere else in Noev:

| Path starts with | Refers to                                             |
| ---------------- | ----------------------------------------------------- |
| a package name   | that package's root module: `qlib::qft::qft`          |
| `pkg`            | the current package's root module: `pkg::ansatz::ry_layer` |
| `super`          | the parent module: `super::helpers`                   |
| `std`, `core`    | the standard library and the intrinsics               |
| a name in scope  | an item declared or imported in this module           |

`pkg` and `super` are contextual keywords: they are only special at the start of a path,
before `::`.

## 3. Imports

`use` brings items into scope:

```noev
use std::observables::{P, PX}
use pkg::ansatz::ry_layer
use qlib::qft::qft as fourier
```

- Braces import several items from one module.
- `as` renames an import.
- There are no glob imports (`use m::*`) at first: every name in a module can then be traced
  to one line, and name resolution stays simple and fast. They can be added later without
  breaking anything.
- Imports are private: `pub use` re-exports an item, which is how a package presents a tidy
  public interface.

## 4. Visibility

Everything is **private to its module** unless marked `pub`:

```noev
pub struct Register {
    pub size: Int          // readable and writable from other modules
    qubits: [Qubit; 8]     // private: only this module's code can touch these qubits

    pub fn new() -> Self { Register { size: 8, qubits: [|0>; 8] } }
    pub fn measure(mv self) -> [Bit; 8] { mv self.qubits.measure() }
}
```

Field privacy matters more in Noev than in most languages. A struct with private qubit fields
controls exactly how its qubits are acted on and consumed. Other modules can only use its
`pub` methods, so it can guarantee invariants such as "these qubits are always measured
together".

Other modules can only move, lend or destructure the fields they can see. A partial move
(N011) of a private field is therefore only possible inside the struct's module.

## 5. Reversibility and unsafety across modules

Reversibility is inferred within a module ([ownership.md](ownership.md) §7). Across modules it
is part of a function's contract:

- A `pub fn` is reversible to other modules **only if it is declared `rev fn`**. Inside its
  own module, the inferred answer still applies.
- So adding a `measure` to a public function's body can never silently break a caller in
  another module that uses it inside `ctrl` or `adj`. The change is caught at the function,
  by its `rev` declaration.
- `unsafe fn` works the same in every module.
- Being a circuit works like reversibility: a `pub fn` is a circuit to other modules only if
  it is declared `circuit fn` ([execution.md](execution.md) §4).

The compiler warns about a `pub fn` that is reversible but not declared `rev`, since callers
elsewhere cannot use it as reversible.

## 6. The prelude

Every module automatically imports the **prelude**: core ([core.md](core.md)) and the
standard library's most common items ([std.md](std.md)). A module's own items and imports take
precedence over prelude names. The rest of the standard library is imported with
`use std::…`.

## 7. Packages and dependencies

`noev.toml` names the package and its dependencies:

```toml
[package]
name = "vqe"
version = "0.1.0"

[dependencies]
qlib = "1.2"
```

- Modules within a package may refer to each other freely, including in cycles.
- Packages may not depend on each other in a cycle.
- `std` and `core` are always available and are not listed.

## 8. Compilation

Each file is lexed and parsed on its own, so files are processed in parallel. Spans keep
their 8-byte size: each file gets its own range of `u32` byte offsets, and the file is found
from the offset (the technique rustc uses). Name resolution then runs over all of a package's
modules together, and type and ownership checking run per function as in v0.1.

## 9. Grammar

```ebnf
item        = [ "pub" ] ( struct_def | fn_def | trait_def | extern_def )
            | impl_def
            | use_decl ;
field       = [ "pub" ] IDENT ":" type ;
member      = field | [ "pub" ] fn_def ;
use_decl    = [ "pub" ] "use" use_tree ;
use_tree    = path_prefix [ "::" ( IDENT [ "as" IDENT ] | "{" use_tree { "," use_tree } [ "," ] "}" ) ]
            | IDENT [ "as" IDENT ] ;
path_prefix = ( IDENT | "pkg" | "super" ) { "::" IDENT } ;
```

`pub`, `use` and `as` become reserved keywords; `pkg` and `super` are contextual.

## 10. Open questions

- **Glob imports**, and whether the prelude itself is one.
- **Visibility levels between private and `pub`**, such as visible within the package only.
- **Package registry and versioning rules**, which belong to the tooling as much as the
  language.
- **Re-export of std items through the prelude**: which standard-library items are common
  enough to be imported automatically.
