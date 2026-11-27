# Conformance tests

End-to-end runtime conformance tests written with Noev's built-in test system
(`test fn`, `test(fail = "…")`, `assert`, `spec/testing.md`).

## Determinism

Every test's assertions are fixed by the language, whatever the backend or random seed: its
measurements have certain outcomes, its distributions have certain outcomes, and its
expectation values are of eigenstates. Anything statistical belongs in an implementation's
own tests.

## Running

Run the entire suite directly with `noev test`:

```
noev test tests/run
noev test --codegen=cranelift tests/run
noev test --codegen=llvm tests/run
```

Cargo's test harnesses (`cargo test -p noev --test run --test codegen --test llvm`) drive
these exact commands.
