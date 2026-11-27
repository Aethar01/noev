# Package tests

Packages of several modules, and their dependencies (`spec/modules.md`). Each directory with
an expected output is run by `cargo test -p noev --test packages`, from this directory:

- `expected.stdout`: `noev run DIR --seed 1` must print exactly this and succeed.
- `expected.stderr`: `noev check DIR --message-format=short` must report exactly this, and
  fail if it reports an error.

Directories without one, such as `qlib`, are dependencies of the others.
