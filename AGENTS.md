# Noev

Hybrid classical–quantum language; compiler written in Rust. The full plan, scope and
milestones live in `docs/PLAN.md` — read it before starting work and update the milestone
table there as milestones complete.

- Source files use the `.nv` extension; CLI binary is `noev` (`check`, `build --emit=qasm3`, `run --shots=N`).
- Build order: parser → types → ownership checker first; IR, OpenQASM 3 codegen and simulator after.
- Speed matters: follow the performance rules in PLAN.md §5 (flat arrays, `u32` indices and spans, no backtracking), and add or update a benchmark when changing a compiler phase.
- Stay inside v0.1 scope (PLAN.md §9 lists what is deferred).
- The language is defined in `spec/`; the compiler implements it. Change the spec first when behaviour changes.
- Ownership errors are the product: every error code in `spec/ownership.md` (N001…) needs a compile-fail test under `tests/ui/`.
- Simulation goes through `ketrs`, the user's own simulator (source in `../ketrs`). Noev depends on a released version (the `v0.1.1` git tag until it is on crates.io), so a change Noev needs must be released in ketrs first. If it is missing something Noev needs, raise it rather than working around it.
