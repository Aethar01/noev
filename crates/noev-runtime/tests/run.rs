//! End-to-end tests: Noev source, checked, lowered and run on ketrs.

use noev_runtime::{Counts, RuntimeError, run};

fn compile(source: &str) -> noev_ir::Program {
    let parsed = noev_syntax::parse(source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let (typed, errors) = noev_sema::check(&parsed.ast);
    assert!(errors.is_empty(), "{errors:?}");
    let errors = noev_sema::check_ownership(&parsed.ast, &typed);
    assert!(errors.is_empty(), "{errors:?}");
    let program = noev_ir::lower(&parsed.ast, &typed, source);
    assert!(noev_ir::validate(&program).is_empty());
    program
}

fn try_run(source: &str, shots: usize) -> Result<Counts, RuntimeError> {
    let program = compile(source);
    run(&program, program.main.expect("a main"), shots, 7)
}

fn counts(source: &str, shots: usize) -> Vec<(String, usize)> {
    try_run(source, shots).unwrap().results
}

/// The single result every shot gives.
fn always(source: &str) -> String {
    let results = counts(source, 50);
    assert_eq!(results.len(), 1, "not deterministic: {results:?}");
    results[0].0.clone()
}

fn float(source: &str) -> f64 {
    always(source).parse().unwrap()
}

#[test]
fn bell_pair_is_half_00_half_11() {
    let results = counts(
        "fn main() -> (Bit, Bit) {\n let a = |+>\n let b = |0>\n ctrl a { b.x() }\n (mv a.measure(), mv b.measure())\n}",
        2000,
    );
    assert_eq!(results.len(), 2, "{results:?}");
    for (result, count) in results {
        assert!(result == "00" || result == "11", "{result}");
        assert!((800..=1200).contains(&count), "{result}: {count}");
    }
}

#[test]
fn returned_qubits_are_sampled() {
    let results = counts(
        "fn main() -> [Qubit; 2] {\n let r = |+0>\n ctrl r[0] { r[1].x() }\n mv r\n}",
        2000,
    );
    assert_eq!(results.len(), 2, "{results:?}");
    for (result, count) in results {
        assert!(result == "00" || result == "11", "{result}");
        assert!((800..=1200).contains(&count), "{result}: {count}");
    }
    assert_eq!(
        always("fn main() -> (Qubit, Bit) {\n let q = |1>\n (mv q, 0)\n}"),
        "10"
    );
}

#[test]
fn returned_qubits_after_feedback_run_per_shot() {
    // The measurement makes shots differ, so each runs separately; the result is still exact.
    assert_eq!(
        always(
            "fn main() -> Qubit {\n let a = |+>\n let b = |0>\n ctrl a { b.x() }\n if mv a.measure() { b.x() }\n mv b\n}"
        ),
        "0"
    );
}

#[test]
fn kets_read_left_to_right() {
    assert_eq!(
        always("fn main() -> [Bit; 3] {\n let r = |101>\n mv r.measure()\n}"),
        "101"
    );
}

#[test]
fn ghz_over_a_loop() {
    let results = counts(
        "fn main() -> [Bit; 4] {\n let r = |0000>\n r[0].h()\n for i in 0..3 { ctrl r[i] { r[i + 1].x() } }\n mv r.measure()\n}",
        500,
    );
    let keys: Vec<&str> = results.iter().map(|(r, _)| r.as_str()).collect();
    assert!(
        keys.iter().all(|k| *k == "0000" || *k == "1111"),
        "{keys:?}"
    );
}

#[test]
fn bras_measure_in_their_basis() {
    assert_eq!(
        always("fn main() -> Bool {\n let q = |+>\n <+| mv q\n}"),
        "true"
    );
    assert_eq!(
        always("fn main() -> Bool {\n let q = |->\n <+| mv q\n}"),
        "false"
    );
    assert_eq!(
        always("fn main() -> Bool {\n let r = |1+>\n <1+| mv r\n}"),
        "true"
    );
}

#[test]
fn adj_undoes_gates_and_functions() {
    assert_eq!(
        always("fn main() -> Bit {\n let q = |+>\n q.t()\n adj q.t()\n q.h()\n mv q.measure()\n}"),
        "0"
    );
    assert_eq!(
        always(
            "fn rot(q: mut Qubit) {\n q.ry(0.7)\n q.rz(0.3)\n q.h()\n}\nfn main() -> Bit {\n let q = |0>\n rot(q)\n adj rot(q)\n mv q.measure()\n}"
        ),
        "0"
    );
}

#[test]
fn controlled_adjoint() {
    // ctrl on |1⟩ applies adj s then s: identity, so h brings |+⟩ back to |0⟩.
    assert_eq!(
        always(
            "fn main() -> Bit {\n let c = |1>\n let q = |+>\n ctrl c { adj q.s() }\n q.s()\n q.h()\n mv c.discard()\n mv q.measure()\n}"
        ),
        "0"
    );
}

#[test]
fn expectation_values_are_exact() {
    assert!((float("fn main() -> Float { <+| X(0) |+> }") - 1.0).abs() < 1e-12);
    assert!(float("fn main() -> Float { <+| Z(0) |+> }").abs() < 1e-12);
    let p11 = float(
        "fn bell() -> [Qubit; 2] {\n let r = |+0>\n ctrl r[0] { r[1].x() }\n mv r\n}\nfn main() -> Float { <bell()| P(0, 1) * P(1, 1) |bell()> }",
    );
    assert!((p11 - 0.5).abs() < 1e-12, "{p11}");
    let zz = float(
        "fn bell() -> [Qubit; 2] {\n let r = |+0>\n ctrl r[0] { r[1].x() }\n mv r\n}\nfn main() -> Float { <bell()| -1.0 * Z(0) * Z(1) + 0.5 * X(0) |bell()> }",
    );
    assert!((zz + 1.0).abs() < 1e-12, "{zz}");
}

#[test]
fn classical_code() {
    assert_eq!(
        always(
            "fn steps(start: Int) -> Int {\n let mut n = start\n let mut k = 0\n while n != 1 {\n if n % 2 == 0 { n /= 2 } else { n = 3 * n + 1 }\n k += 1\n }\n k\n}\nfn main() -> Int { steps(27) }"
        ),
        "111"
    );
    assert_eq!(
        always(
            "fn bump(n: mut Int) { n += 1 }\nfn main() -> Int {\n let mut n = 1\n bump(mut n)\n bump(mut n)\n n\n}"
        ),
        "3"
    );
    assert_eq!(
        always("fn main() -> Int { (2.7).round() + Int(Bool(Bit(true))) }"),
        "4"
    );
}

#[test]
fn measurement_feedback() {
    assert_eq!(
        always(
            "fn main() -> Bit {\n let a = |+>\n let b = |0>\n ctrl a { b.x() }\n if mv a.measure() { b.x() }\n mv b.measure()\n}"
        ),
        "0"
    );
}

#[test]
fn structs_and_methods() {
    let source = "struct E {\n angle: Float\n state: Qubit\n fn new(a: Float) -> Self { E { angle: a, state: |0> } }\n fn rotate(mut self) -> mut Self {\n self.state.ry(self.angle)\n mut self\n }\n fn finish(mv self) -> Bit { mv self.state.measure() }\n}\nfn main() -> Bit {\n let mut e = E::new(Float::PI)\n e.rotate().rotate()\n mv e.finish()\n}";
    assert_eq!(always(source), "0");
}

#[test]
fn qubits_are_reused() {
    // A thousand qubits one after another fit in a one-qubit state.
    assert_eq!(
        always(
            "fn main() -> Int {\n let mut ones = 0\n for i in 0..1000 {\n let q = |1>\n if mv q.measure() { ones += 1 }\n }\n ones\n}"
        ),
        "1000"
    );
}

#[test]
fn release_is_checked_on_the_simulator() {
    let error = try_run(
        "fn main() {\n let q = |1>\n unsafe { mv q.release() }\n}",
        1,
    )
    .unwrap_err();
    assert!(error.message.contains("not in |0⟩"), "{}", error.message);
    assert!(
        try_run(
            "fn main() {\n let q = |1>\n q.x()\n unsafe { mv q.release() }\n}",
            1
        )
        .is_ok()
    );
}

#[test]
fn failed_assertions_stop_the_program() {
    let error = try_run("fn main() {\n let n = 3\n assert n < 2\n}", 1).unwrap_err();
    assert_eq!(error.message, "assertion failed");
    assert!(error.span.is_some());
}

#[test]
fn several_controls_need_ketrs_support() {
    let error = try_run(
        "fn main() -> Bit {\n let a = |1>\n let b = |1>\n let c = |0>\n ctrl a, b { c.x() }\n mv a.discard()\n mv b.discard()\n mv c.measure()\n}",
        1,
    )
    .unwrap_err();
    assert!(
        error.message.contains("multi-controlled"),
        "{}",
        error.message
    );
}
