use noev_syntax::ast::{ExprKind, ItemKind, StmtKind};
use noev_syntax::parse;

use super::*;

fn errors(source: &str) -> Vec<String> {
    let parsed = parse(source);
    assert!(
        parsed.diagnostics.is_empty(),
        "syntax errors: {:?}",
        parsed.diagnostics
    );
    let (_, diagnostics) = check(&parsed.ast);
    diagnostics.into_iter().map(|d| d.message).collect()
}

fn ok(source: &str) {
    let errs = errors(source);
    assert!(errs.is_empty(), "{errs:?}");
}

fn one_error(source: &str) -> String {
    let errs = errors(source);
    assert_eq!(errs.len(), 1, "{errs:?}");
    errs.into_iter().next().unwrap()
}

#[test]
fn mismatched_types() {
    assert_eq!(
        one_error("fn f() -> Int { 1.0 }"),
        "mismatched types: expected `Int`, found `Float`"
    );
    assert_eq!(
        one_error("fn f() -> Float { 1.0 + 2 }"),
        "`+` cannot be applied to `Float` and `Int`"
    );
    assert_eq!(
        one_error("fn f(x: Float) -> Bool { x > 0 }"),
        "`>` cannot be applied to `Float` and `Int`"
    );
}

#[test]
fn bit_literals() {
    ok("fn f() -> Bit { let b: Bit = 1\n b }");
    ok("fn f(m: Bit) -> Bool { m == 1 }");
    ok("fn f(m: Bit) -> Bool { 0 == m }");
    assert_eq!(
        one_error("fn f() -> Bit { 2 }"),
        "mismatched types: expected `Bit`, found `Int`"
    );
}

#[test]
fn unknown_names() {
    assert_eq!(
        one_error("fn f() -> Int { y }"),
        "cannot find `y` in this scope"
    );
    assert_eq!(one_error("fn f() { g() }"), "cannot find the function `g`");
    assert_eq!(
        one_error("fn f(x: Thing) {}"),
        "cannot find the type `Thing`"
    );
    assert_eq!(
        one_error("fn f() -> Float { Float::TWO }"),
        "cannot find the value `Float::TWO`"
    );
    assert_eq!(
        one_error("fn g() {}\nfn f() { let x = g }"),
        "`g` is a function; call it with `g(…)`"
    );
}

#[test]
fn structs() {
    let s = "struct E { angle: Float\n state: Qubit\n fn new(a: Float) -> Self { E { angle: a, state: |0> } } }\n";
    ok(&format!(
        "{s}fn f() -> Float {{ let e = E::new(1.0)\n let b = mv e.state.measure()\n e.angle }}"
    ));
    assert_eq!(
        one_error(&format!("{s}fn f() -> E {{ E {{ angle: 1.0 }} }}")),
        "missing fields: `state`"
    );
    assert_eq!(
        one_error(&format!("{s}fn f(e: ref E) -> Float {{ e.size }}")),
        "`E` has no field `size`"
    );
    assert_eq!(
        one_error(&format!("{s}fn f(e: ref E) {{ e.spin() }}")),
        "`E` has no method `spin`"
    );
    assert_eq!(
        one_error(&format!("{s}fn f(e: ref E) {{ e.new(1.0) }}")),
        "`new` is an associated function, not a method; call it as `E::new(…)`"
    );
}

#[test]
fn quantum_rules() {
    assert_eq!(
        one_error("fn f() { let q = |0>\n if q { } }"),
        "a qubit cannot be a condition"
    );
    assert_eq!(
        one_error("fn f(b: Bool) { ctrl b { } }"),
        "`ctrl` needs qubits as controls, but this is `Bool`"
    );
    assert_eq!(
        one_error("fn f() -> Bool { let q = |0>\n <01| mv q }"),
        "this bra tests 2 qubits, so it needs `[Qubit; 2]`, but the value is `Qubit`"
    );
    assert_eq!(
        one_error("fn n(x: Int) -> Int { x }\nfn f() -> Float { <n(1)| Z(0) |n(1)> }"),
        "a preparation must produce qubits, but this produces `Int`"
    );
    assert_eq!(
        one_error("fn f() -> Float { <+| 1.0 |+> }"),
        "mismatched types: expected `Observable`, found `Float`"
    );
    assert_eq!(
        one_error("fn f() { let q = |0>\n let r = [q; 3] }"),
        "only kets can be repeated: anything else would be copied"
    );
}

#[test]
fn observables() {
    ok("fn f() -> Observable { -1.0 * Z(0) * Z(1) + 0.5 * (X(0) + X(1)) - I() }");
    ok("fn f() -> Observable { P(0, 1) * PX(1, 0) }");
}

#[test]
fn control_flow() {
    assert_eq!(
        one_error("fn f() -> Int { let x = 1 }"),
        "`f` must return `Int`, but its body ends without a value"
    );
    ok("fn f(c: Bool) -> Int { if c { return 1 }\n return 2 }");
    ok("fn f(c: Bool) -> Int { if c { 1 } else { 2 } }");
    assert_eq!(one_error("fn f() { break }"), "`break` outside a loop");
    assert_eq!(
        one_error("fn f(c: Bool) -> Int { let x = if c { 1 }\n x }"),
        "an `if` without `else` has no value when the condition is false"
    );
    assert_eq!(
        one_error("fn f(c: Bool) -> Int { if c { 1 } else { 2.0 } }"),
        "mismatched types: expected `Int`, found `Float`"
    );
}

#[test]
fn calls_and_conversions() {
    assert_eq!(
        one_error("fn g(a: Int) {}\nfn f() { g(1, 2) }"),
        "this takes 1 argument, but 2 were given"
    );
    assert_eq!(
        one_error("fn f(x: Float) -> Int { Int(x) }"),
        "`Float` cannot be converted to `Int`"
    );
    ok("fn f(n: Int, b: Bit) -> Float { Float(n) + Float(Int(b)) + Float::sin(1.0) + 2.0.sqrt() }");
    assert_eq!(
        one_error("fn f() -> Bit { let q = 1\n measure(mv q) }"),
        "`measure` takes a qubit or an array of qubits, but this is `Int`"
    );
}

#[test]
fn strings_and_printing() {
    ok(
        "fn main() {\n let x = 3\n let s = \"x = {x:>4}, pi = {Float::PI:.3}, mask {x:08b}\"\n print(s, x, 1.5, true)\n eprint(\"done\")\n}",
    );
    ok("fn f(a: String, b: String) -> Bool { a == b }");
    assert_eq!(
        one_error("fn f(x: Int) -> String { \"{x:.2}\" }"),
        "this format spec does not apply to `Int`"
    );
    assert_eq!(
        one_error("fn f(x: Float) -> String { \"{x:b}\" }"),
        "this format spec does not apply to `Float`"
    );
    assert_eq!(
        one_error("fn main() {\n let q = |0>\n print(q)\n mv q.discard()\n}"),
        "`Qubit` cannot be printed"
    );
    assert_eq!(
        one_error("fn f() -> String { \"{y}\" }"),
        "cannot find `y` in this scope"
    );
}

#[test]
fn main_takes_nothing_and_returns_unit() {
    assert_eq!(
        one_error("fn main() -> Int { 1 }"),
        "`main` must take no parameters and return `()`"
    );
    assert_eq!(
        one_error("fn main(n: Int) {}"),
        "`main` must take no parameters and return `()`"
    );
    ok("fn main() {}");
}

#[test]
fn reserved_items() {
    assert_eq!(
        one_error("trait T { fn m(ref self) }"),
        "traits are not supported yet"
    );
    assert_eq!(
        one_error("fn f() { with { } do { } }"),
        "`with`/`do` is not supported yet"
    );
}

#[test]
fn records_implicit_lends() {
    let source = "fn f() { let q = |0>\n h(q)\n mv q.discard() }";
    let parsed = parse(source);
    let (typed, diagnostics) = check(&parsed.ast);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let ItemKind::Fn(f) = parsed.ast.item(parsed.ast.list(parsed.ast.root)[0]).kind else {
        panic!()
    };
    let body = parsed.ast.block(parsed.ast.function(f).body.unwrap());
    let StmtKind::Expr { expr: call, .. } = parsed.ast.stmt(parsed.ast.list(body.stmts)[1]).kind
    else {
        panic!()
    };
    let ExprKind::Call { args, .. } = parsed.ast.expr(call).kind else {
        panic!()
    };
    let arg = parsed.ast.list(args)[0];
    assert_eq!(typed.adjust[arg.index()], Adjust::Lend { mutable: true });
    assert_eq!(typed.ty(arg), Ty::QUBIT);
}

#[test]
fn option_and_result() {
    ok("fn f(c: Bool) -> Option<Int> { if c { Some(1) } else { None } }");
    ok("fn f(b: Int) -> Result<Int, String> { if b == 0 { Err(\"zero\") } else { Ok(6 / b) } }");
    ok("fn f(o: Option<Int>) -> Int { match o { Some(n) => n, None => 0 } }");
    ok("fn f() -> Bool { let n: Option<Bit> = None\n n == Some(1) }");
    assert_eq!(
        one_error("fn f() { let x = None }"),
        "cannot tell which `Option` `None` is"
    );
    assert_eq!(
        one_error("fn f() -> Int { None }"),
        "mismatched types: expected `Int`, found `None`, a variant of `Option`"
    );
    assert_eq!(
        one_error("fn f(o: Option<Int>) -> Int { match o { Some(n) => n } }"),
        "this `match` does not cover `None`"
    );
    assert_eq!(
        one_error("fn f(o: Option<Int>) -> Int { match o { Ok(n) => n, _ => 0 } }"),
        "`Ok` is a variant of `Result`, but the value is `Option<Int>`"
    );
    assert_eq!(
        one_error("fn f(o: Option) {}"),
        "`Option` needs its type arguments, as in `Option<Int>`"
    );
    assert_eq!(
        one_error("fn f(r: Result<Int>) {}"),
        "`Result` takes two type arguments"
    );
}

#[test]
fn ui_tests_type_check_cleanly() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/ui");
    let mut stack = vec![std::path::PathBuf::from(dir)];
    let mut count = 0;
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            stack.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
        } else if path.extension().is_some_and(|e| e == "nv") {
            let source = std::fs::read_to_string(&path).unwrap();
            let (_, diagnostics) = check(&parse(&source).ast);
            assert!(
                diagnostics.is_empty(),
                "{}: {diagnostics:?}",
                path.display()
            );
            count += 1;
        }
    }
    assert!(count > 0);
}
