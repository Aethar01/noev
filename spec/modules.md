# Modules and packages

Status: **Implemented**, except where a section says otherwise: naming the prelude itself
as `std::prelude` (§6).
`noev check`, `build`, `run` and `export` take a `.nv` file or a package directory.

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
module. A program written before modules stays valid, except that standard-library items
outside the prelude (§6) now need a `use`; the error for such a name suggests the import.

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

- Braces import several items from one module; `self` in braces imports the module itself
  (`use std::observables::{self, P}`).
- `as` renames an import; `as _` imports a trait's methods without its name.
- `use m::*` (a **glob import**) imports every item of `m` visible here. Names declared in
  the module and names imported explicitly take precedence over glob-imported ones. Two
  glob imports that bring the same name are an error only where that name is used.
- Imports are private unless marked: `pub use` re-exports an item, and `pub use m::*` every
  item of `m` visible to the re-export's users, which is how a package presents a tidy public
  interface.

These are Rust's rules.

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

Between private and `pub`, as in Rust, an item can be visible to part of the package:

| Visibility        | Visible in                                                  |
| ----------------- | ----------------------------------------------------------- |
| (none)            | its own module and the modules inside it                    |
| `pub(super)`      | its parent module and the modules inside it                 |
| `pub(in path)`    | the named module, which must contain this one, and the modules inside it |
| `pub(pkg)`        | the whole package (Rust's `pub(crate)`)                     |
| `pub`             | everywhere                                                  |

`pub(self)` is the same as no marker. A visible item whose signature names a less visible
type gets a warning, as with Rust's `private_interfaces` lint.

Field privacy matters more in Noev than in most languages. A struct with private qubit fields
controls exactly how its qubits are acted on and consumed. Other modules can only use its
`pub` methods, so it can guarantee invariants such as "these qubits are always measured
together".

Other modules can only move, lend or destructure the fields they can see. A partial move
(N011) of a private field is therefore only possible inside the struct's module.

## 5. Reversibility and unsafety across modules

Reversibility is inferred within a module ([ownership.md](ownership.md) §7). Across modules it
is part of a function's contract:

- A function is reversible to other modules **only if it is declared `rev fn`**, whatever
  its visibility (`pub`, `pub(pkg)`, …). Inside its own module, the inferred answer still
  applies.
- So adding a `measure` to a public function's body can never silently break a caller in
  another module that uses it inside `ctrl` or `adj`. The change is caught at the function,
  by its `rev` declaration.
- `unsafe fn` works the same in every module.
- Being a circuit does not work like reversibility: the compiler decides it across modules
  and packages, from the bodies of the functions a run calls ([execution.md](execution.md)
  §4), as it does within one. A change inside a function can therefore switch a run in
  another module between one circuit and shot by shot, but never what the run computes:
  `--emit=plan` says why a run is not a circuit, and a backend that takes only circuits
  rejects it when the program is built. `circuit fn` still promises it for one function.
  When packages are compiled separately, what the compiler found is kept with each.

The compiler warns about a function visible outside its module that acts on qubits and is
reversible but not declared `rev`, since callers elsewhere cannot use it as reversible.
Functions that do not act on qubits are left alone, so classical helpers need no `rev`
unless reversible code in other modules calls them.

## 6. The prelude

Every module automatically imports the **prelude**, the module `std::prelude`, as Rust does.
Its names have the lowest precedence: a module's own items, its explicit imports and its glob
imports all take precedence over them. The rest of the standard library is imported with
`use std::…`.

The prelude holds what almost every program uses, as Rust's does:

- every intrinsic in [core.md](core.md): gates, `measure`, `reset`, measurement tests,
  observables, maths;
- `discard`, `Basis` and `measure_in`, `print` and `eprint` ([std.md](std.md));
- the built-in types and `Option` and `Result` with their variants ([types.md](types.md));
- once traits exist, `Copy`, `Discard`, `Measure` and `From`.

The other standard-library items live in modules of their own:

| Item                  | Module              |
| --------------------- | ------------------- |
| `State`               | `std::state`        |
| `amplitudes`          | `std::debug`        |
| `fft`, `ifft`         | `std::fourier`      |
| `P`, `PX`             | `std::observables`  |

A name used without its import is an error whose fix-it is the `use` line. (Not implemented
yet: naming the prelude itself, as in `use std::prelude::print`, and paths through `core`.)

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
- Versions follow semantic versioning, and requirements mean what they mean in Cargo:
  `"1.2"` accepts `1.2.0` up to, not including, `2.0.0`.

## 8. Compilation

Each file is lexed and parsed on its own, so files can be processed in parallel (for now
they are parsed one after another, into one syntax tree for the package). Spans keep
their 8-byte size: each file gets its own range of `u32` byte offsets, and the file is found
from the offset (the technique rustc uses). Name resolution then runs over all of a package's
modules together, and type and ownership checking run per function as they do now.

## 9. Grammar

```ebnf
item        = [ vis ] ( struct_def | fn_def | trait_def | extern_def )
            | impl_def
            | use_decl ;
vis         = "pub" [ "(" ( "pkg" | "super" | "self" | "in" path_prefix ) ")" ] ;
field       = [ vis ] IDENT ":" type ;
member      = field | [ vis ] fn_def ;
use_decl    = [ vis ] "use" use_tree ;
use_tree    = path_prefix [ "::" ( use_leaf | "*" | "{" use_tree { "," use_tree } [ "," ] "}" ) ]
            | use_leaf ;
use_leaf    = ( IDENT | "self" ) [ "as" ( IDENT | "_" ) ] ;
path_prefix = ( IDENT | "pkg" | "super" ) { "::" IDENT } ;
```

`pub`, `use` and `as` become reserved keywords; `pkg` and `super` are contextual. After
`pub`, a `(` always starts a visibility, as in Rust.

## 10. Open questions

- **Package registry**: where packages are published and fetched from, which belongs to the
  tooling as much as the language. Until there is one, dependencies are paths or git URLs,
  as Cargo allows.
