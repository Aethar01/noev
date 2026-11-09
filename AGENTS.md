# Noev

Hybrid classical–quantum language; compiler written in Rust. The full plan, scope and
milestones live in `docs/PLAN.md` — read it before starting work and update the milestone
table there as milestones complete.

- Source files use the `.nv` extension; CLI binary is `noev` (`check`, `build --emit=qasm3`, `run --shots=N`).
- Build order: parser → types → ownership checker first; IR, OpenQASM 3 codegen and simulator after.
- Speed matters: performance is a high priority and add or update a benchmark when changing a compiler phase.
- The language is defined in `spec/`; the compiler implements it. Change the spec first when behaviour changes.
- Ownership errors are the product: every error code in `spec/ownership.md` (N001…) needs a compile-fail test under `tests/ui/`.
- Simulation goes through `ketrs`, the user's own simulator (source in `../ketrs`), an optional dependency from crates.io behind the `ketrs` feature (on by default): it is one backend, so Noev must build without it (`--no-default-features`). A change Noev needs must be released in ketrs first. If it is missing something Noev needs, raise it rather than working around it.
