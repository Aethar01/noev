# UI tests

The conformance suite for Noev's compile-time checks. Every implementation of Noev must pass
it, so it depends only on the `noev` command line, not on any compiler internals.

## Layout

- `nNNN_*.nv`: programs that must be rejected. Each covers one error code from
  `spec/ownership.md`.
- `pass/*.nv`: programs that must be accepted with no errors.

## Expected errors

A comment `//~ N001` marks a line where error `N001` must be reported. Several codes can be
listed (`//~ N005 N005`). Every expected error must be reported on its line, and no other
errors may be reported anywhere in the file. Warnings are ignored.

Tests assume the checker recovers from each error as if the program had been written the
way the help suggests (a missing `mv` counts as written, a rejected move counts as having
happened), so one mistake does not cause follow-on errors. A rejected move of `r[i]` (N012)
counts as moving the whole array.

## Running

`cargo test -p noev --test ui` runs every file through:

```
noev check --message-format=short FILE
```

which must print one line per diagnostic to stderr:

```
FILE:LINE:COLUMN: error[N001]: message
```

and exit with status 0 when there are no errors, 1 when there are, and 2 if it could not
check the file at all.
