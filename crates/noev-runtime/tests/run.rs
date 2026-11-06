//! End-to-end tests: Noev source, checked, lowered and run on ketrs.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use noev_runtime::sampling::sample;
use noev_runtime::{Backend, Counts, Ketrs, Output, RuntimeError, run, run_main};

fn compile(source: &str) -> noev_ir::Program {
    let parsed = noev_syntax::parse(source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let (typed, errors) = noev_sema::check(&parsed.ast);
    assert!(errors.is_empty(), "{errors:?}");
    let errors = noev_sema::check_ownership(&parsed.ast, &typed);
    assert!(errors.is_empty(), "{errors:?}");
    let (circuits, _) = noev_sema::check_circuits(&parsed.ast, &typed);
    let program = noev_ir::lower(&parsed.ast, &typed, &circuits, source);
    assert!(noev_ir::validate(&program).is_empty());
    program
}

fn try_run(source: &str, shots: usize) -> Result<Counts, RuntimeError> {
    let program = compile(source);
    let test = program
        .functions
        .iter()
        .position(|f| f.name == "test")
        .expect("a `fn test`");
    run(&program, noev_ir::ir::FuncId(test as u32), shots, 7)
}

/// A buffer the tests can read back after the program has written to it.
#[derive(Clone, Default)]
struct Buffer(Rc<RefCell<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `main` and returns (standard output, standard error).
fn output(source: &str) -> (String, String) {
    let program = compile(source);
    let (out, err) = (Buffer::default(), Buffer::default());
    run_main(
        &program,
        &Ketrs,
        7,
        Output {
            stdout: Box::new(out.clone()),
            stderr: Box::new(err.clone()),
        },
    )
    .unwrap();
    let text = |b: Buffer| String::from_utf8(b.0.borrow().clone()).unwrap();
    (text(out), text(err))
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
        "fn test() -> (Bit, Bit) {\n let a = |+>\n let b = |0>\n ctrl a { b.x() }\n (mv a.measure(), mv b.measure())\n}",
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
        "fn test() -> [Qubit; 2] {\n let r = |+0>\n ctrl r[0] { r[1].x() }\n mv r\n}",
        2000,
    );
    assert_eq!(results.len(), 2, "{results:?}");
    for (result, count) in results {
        assert!(result == "00" || result == "11", "{result}");
        assert!((800..=1200).contains(&count), "{result}: {count}");
    }
    assert_eq!(
        always("fn test() -> (Qubit, Bit) {\n let q = |1>\n (mv q, 0)\n}"),
        "10"
    );
}

#[test]
fn returned_qubits_after_feedback_run_per_shot() {
    // The measurement makes shots differ, so each runs separately; the result is still exact.
    assert_eq!(
        always(
            "fn test() -> Qubit {\n let a = |+>\n let b = |0>\n ctrl a { b.x() }\n if mv a.measure() { b.x() }\n mv b\n}"
        ),
        "0"
    );
}

#[test]
fn kets_read_left_to_right() {
    assert_eq!(
        always("fn test() -> [Bit; 3] {\n let r = |101>\n mv r.measure()\n}"),
        "101"
    );
}

#[test]
fn ghz_over_a_loop() {
    let results = counts(
        "fn test() -> [Bit; 4] {\n let r = |0000>\n r[0].h()\n for i in 0..3 { ctrl r[i] { r[i + 1].x() } }\n mv r.measure()\n}",
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
        always("fn test() -> Bool {\n let q = |+>\n <+| mv q\n}"),
        "true"
    );
    assert_eq!(
        always("fn test() -> Bool {\n let q = |->\n <+| mv q\n}"),
        "false"
    );
    assert_eq!(
        always("fn test() -> Bool {\n let r = |1+>\n <1+| mv r\n}"),
        "true"
    );
}

#[test]
fn adj_undoes_gates_and_functions() {
    assert_eq!(
        always("fn test() -> Bit {\n let q = |+>\n q.t()\n adj q.t()\n q.h()\n mv q.measure()\n}"),
        "0"
    );
    assert_eq!(
        always(
            "fn rot(q: mut Qubit) {\n q.ry(0.7)\n q.rz(0.3)\n q.h()\n}\nfn test() -> Bit {\n let q = |0>\n rot(q)\n adj rot(q)\n mv q.measure()\n}"
        ),
        "0"
    );
}

#[test]
fn controlled_adjoint() {
    // ctrl on |1⟩ applies adj s then s: identity, so h brings |+⟩ back to |0⟩.
    assert_eq!(
        always(
            "fn test() -> Bit {\n let c = |1>\n let q = |+>\n ctrl c { adj q.s() }\n q.s()\n q.h()\n mv c.discard()\n mv q.measure()\n}"
        ),
        "0"
    );
}

#[test]
fn expectation_values_are_exact() {
    assert!((float("fn test() -> Float { <+| X(0) |+> }") - 1.0).abs() < 1e-12);
    assert!(float("fn test() -> Float { <+| Z(0) |+> }").abs() < 1e-12);
    let p11 = float(
        "fn bell() -> [Qubit; 2] {\n let r = |+0>\n ctrl r[0] { r[1].x() }\n mv r\n}\nfn test() -> Float { <bell()| P(0, 1) * P(1, 1) |bell()> }",
    );
    assert!((p11 - 0.5).abs() < 1e-12, "{p11}");
    let zz = float(
        "fn bell() -> [Qubit; 2] {\n let r = |+0>\n ctrl r[0] { r[1].x() }\n mv r\n}\nfn test() -> Float { <bell()| -1.0 * Z(0) * Z(1) + 0.5 * X(0) |bell()> }",
    );
    assert!((zz + 1.0).abs() < 1e-12, "{zz}");
}

#[test]
fn classical_code() {
    assert_eq!(
        always(
            "fn steps(start: Int) -> Int {\n let mut n = start\n let mut k = 0\n while n != 1 {\n if n % 2 == 0 { n /= 2 } else { n = 3 * n + 1 }\n k += 1\n }\n k\n}\nfn test() -> Int { steps(27) }"
        ),
        "111"
    );
    assert_eq!(
        always(
            "fn bump(n: mut Int) { n += 1 }\nfn test() -> Int {\n let mut n = 1\n bump(mut n)\n bump(mut n)\n n\n}"
        ),
        "3"
    );
    assert_eq!(
        always("fn test() -> Int { (2.7).round() + Int(Bool(Bit(true))) }"),
        "4"
    );
}

#[test]
fn measurement_feedback() {
    assert_eq!(
        always(
            "fn test() -> Bit {\n let a = |+>\n let b = |0>\n ctrl a { b.x() }\n if mv a.measure() { b.x() }\n mv b.measure()\n}"
        ),
        "0"
    );
}

#[test]
fn structs_and_methods() {
    let source = "struct E {\n angle: Float\n state: Qubit\n fn new(a: Float) -> Self { E { angle: a, state: |0> } }\n fn rotate(mut self) -> mut Self {\n self.state.ry(self.angle)\n mut self\n }\n fn finish(mv self) -> Bit { mv self.state.measure() }\n}\nfn test() -> Bit {\n let mut e = E::new(Float::PI)\n e.rotate().rotate()\n mv e.finish()\n}";
    assert_eq!(always(source), "0");
}

#[test]
fn qubits_are_reused() {
    // A thousand qubits one after another fit in a one-qubit state.
    assert_eq!(
        always(
            "fn test() -> Int {\n let mut ones = 0\n for i in 0..1000 {\n let q = |1>\n if mv q.measure() { ones += 1 }\n }\n ones\n}"
        ),
        "1000"
    );
}

#[test]
fn release_is_checked_on_the_simulator() {
    let error = try_run(
        "fn test() {\n let q = |1>\n unsafe { mv q.release() }\n}",
        1,
    )
    .unwrap_err();
    assert!(error.message.contains("not in |0⟩"), "{}", error.message);
    assert!(
        try_run(
            "fn test() {\n let q = |1>\n q.x()\n unsafe { mv q.release() }\n}",
            1
        )
        .is_ok()
    );
}

#[test]
fn failed_assertions_stop_the_program() {
    let error = try_run("fn test() {\n let n = 3\n assert n < 2\n}", 1).unwrap_err();
    assert_eq!(error.message, "assertion failed");
    assert!(error.span.is_some());
}

#[test]
fn multi_controlled_gates() {
    // Toffoli: c flips only when a and b are both 1.
    let toffoli = |a: &str, b: &str| {
        always(&format!(
            "fn test() -> (Bit, Bit, Bit) {{\n let a = |{a}>\n let b = |{b}>\n let c = |0>\n ctrl a, b {{ c.x() }}\n (mv a.measure(), mv b.measure(), mv c.measure())\n}}"
        ))
    };
    assert_eq!(toffoli("0", "0"), "000");
    assert_eq!(toffoli("1", "0"), "100");
    assert_eq!(toffoli("1", "1"), "111");
    // Nested ctrl and a whole register as the control.
    assert_eq!(
        always(
            "fn test() -> [Bit; 4] {\n let r = |1110>\n ctrl r[0] { ctrl r[1], r[2] { r[3].x() } }\n mv r.measure()\n}"
        ),
        "1111"
    );
    assert_eq!(
        always(
            "fn test() -> Bit {\n let r = |11>\n let t = |0>\n ctrl r { t.x() }\n let rs = mv r.measure()\n mv t.measure()\n}"
        ),
        "1"
    );
    // Fredkin: swap under a control.
    assert_eq!(
        always(
            "fn test() -> [Bit; 3] {\n let r = |110>\n ctrl r[0] { swap(r[1], r[2]) }\n mv r.measure()\n}"
        ),
        "101"
    );
}

#[test]
fn reversible_majority_with_adj() {
    // The program from tests/ui/pass/reversible.nv: majority then adj majority restores |110⟩.
    assert_eq!(
        always(
            "fn majority(a: mut Qubit, b: mut Qubit, c: mut Qubit) {\n ctrl c { b.x(); a.x() }\n ctrl a, b { c.x() }\n}\nfn test() -> [Bit; 3] {\n let r = |110>\n majority(r[0], r[1], r[2])\n adj majority(r[0], r[1], r[2])\n mv r.measure()\n}"
        ),
        "110"
    );
}

#[test]
fn printing() {
    let (out, err) = output(
        "struct P { x: Int\n y: Float }\nfn main() {\n let r = |01>\n let bits = mv r.measure()\n print(\"bits:\", bits, 3, -1.0, true, (1, 2.5), [4, 5], P { x: 1, y: 2.0 })\n print()\n eprint(\"warning\")\n}",
    );
    assert_eq!(
        out,
        "bits: 01 3 -1.0 true (1, 2.5) [4, 5] P { x: 1, y: 2.0 }\n\n"
    );
    assert_eq!(err, "warning\n");
}

#[test]
fn interpolation() {
    let (out, _) = output(
        "fn main() {\n let n = 5\n let f = 0.126\n print(\"n={n:>3} f={f:.2} b={n:08b} hex={255:x} {{literal} {Float::PI:+.3}\")\n}",
    );
    assert_eq!(out, "n=  5 f=0.13 b=00000101 hex=ff {literal} +3.142\n");
}

#[test]
fn strings_compare() {
    let (out, _) = output("fn main() {\n let a = \"x{1}\"\n print(a == \"x1\", a != \"x1\")\n}");
    assert_eq!(out, "true false\n");
}

/// Samples `fn test()` through the language's `sample`, returning (outcome counts, whether the
/// circuit path was taken).
fn sampled(source: &str, shots: usize) -> (Vec<(String, usize)>, bool) {
    let program = compile(source);
    let test = program
        .functions
        .iter()
        .position(|f| f.name == "test")
        .expect("a `fn test`");
    let dist = sample(
        &program,
        &Ketrs,
        noev_ir::ir::FuncId(test as u32),
        Vec::new(),
        shots,
        11,
        |v| v.to_string(),
    )
    .unwrap();
    (
        dist.outcomes
            .iter()
            .map(|o| (o.key.clone(), o.count))
            .collect(),
        dist.from_circuit,
    )
}

#[test]
fn circuits_are_sampled_from_one_simulation() {
    let (bell, circuit) = sampled(
        "fn test() -> (Bit, Bit) {\n let a = |+>\n let b = |0>\n ctrl a { b.x() }\n (mv a.measure(), mv b.measure())\n}",
        2000,
    );
    assert!(circuit);
    assert_eq!(bell.len(), 2, "{bell:?}");
    assert!(
        bell.iter()
            .all(|(k, n)| (k == "00" || k == "11") && (800..=1200).contains(n)),
        "{bell:?}"
    );
}

#[test]
fn post_processing_is_replayed_per_outcome() {
    // Counting ones after the last quantum operation: binomial(3, 1/2).
    let (counts, circuit) = sampled(
        "fn test() -> Int {\n let r = |+++>\n let bits = mv r.measure()\n let mut ones = 0\n for i in 0..3 { if bits[i] { ones += 1 } }\n ones\n}",
        8000,
    );
    assert!(circuit);
    let get = |k: &str| {
        counts
            .iter()
            .find(|(key, _)| key == k)
            .map_or(0, |(_, n)| *n)
    };
    assert!(
        (800..=1200).contains(&get("0")) && (2700..=3300).contains(&get("1")),
        "{counts:?}"
    );
    assert!(
        (2700..=3300).contains(&get("2")) && (800..=1200).contains(&get("3")),
        "{counts:?}"
    );
}

#[test]
fn circuit_and_shot_paths_agree() {
    // Ctrl, adj, discard and a mid-circuit measurement, by both paths: the same program, made
    // not a circuit by converting a result to a number before a later gate.
    let body = "let r = |+0+>\n ctrl r[0] { r[1].x() }\n r[2].t()\n adj r[2].t()\n r[2].h()\n let extra = |+>\n mv extra.discard()\n let m = mv r[2].measure()";
    let source = format!(
        "fn test() -> (Bit, Bit, Bit) {{\n {body}\n (m, mv r[0].measure(), mv r[1].measure())\n}}"
    );
    let (a, fast) = sampled(&source, 4000);
    assert!(fast);
    let slow_source = source.replace("let m = mv r[2].measure()", "let m = mv r[2].measure()\n let k = Int(m)\n let y = |0>\n y.ry(Float(k))\n mv y.discard()");
    let (b, slow) = sampled(&slow_source, 4000);
    assert!(!slow);
    // Both give m = 0 (t then adj t cancel, and h takes |+⟩ to |0⟩), and 00 or 11 for the
    // pair; three bits print as a bit string.
    for counts in [&a, &b] {
        assert_eq!(counts.len(), 2, "{counts:?}");
        assert!(
            counts
                .iter()
                .all(|(k, n)| (k == "000" || k == "011") && (1700..=2300).contains(n)),
            "{counts:?}"
        );
    }
}

#[test]
fn feedback_ifs_are_part_of_the_circuit() {
    let (counts, circuit) = sampled(
        "fn test() -> Bit {\n let a = |+>\n let b = |0>\n ctrl a { b.x() }\n if mv a.measure() { b.x() }\n mv b.measure()\n}",
        200,
    );
    assert!(circuit);
    assert_eq!(counts, [("0".to_string(), 200)]);
}

/// Teleportation: the corrections depend on two measurements, combined with logic, and the
/// teleported state comes out exactly.
const TELEPORT: &str = "
fn correct(q: mut Qubit, flip: Bit, phase: Bool) {
    if phase { q.z() }
    if flip == 1 and not false { q.x() }
}
fn test() -> (Bit, Bit, Bit) {
    let msg = |0>
    msg.ry(2.0 * Float::PI / 3.0)
    let a = |+>
    let b = |0>
    ctrl a { b.x() }
    ctrl msg { a.x() }
    msg.h()
    let m1 = mv msg.measure()
    let m2 = mv a.measure()
    correct(b, m2, m1 == 1)
    // Undo the preparation: b should now be |0⟩.
    b.ry(-2.0 * Float::PI / 3.0)
    (m1, m2, mv b.measure())
}";

#[test]
fn teleportation_is_one_circuit() {
    let (counts, circuit) = sampled(TELEPORT, 4000);
    assert!(circuit);
    assert!(counts.iter().all(|(k, _)| k.ends_with('0')), "{counts:?}");
    assert_eq!(counts.len(), 4, "{counts:?}");
}

#[test]
fn results_passed_to_functions_that_misuse_them_are_not_circuits() {
    // `rotate` turns its `Bit` into an angle, so given a measurement result it breaks the rule.
    let (counts, circuit) = sampled(
        "fn rotate(q: mut Qubit, b: Bit) { q.ry(Float(Int(b)) * Float::PI) }\nfn test() -> Bit {\n let a = |1>\n let m = mv a.measure()\n let q = |0>\n rotate(q, m)\n mv q.measure()\n}",
        50,
    );
    assert!(!circuit);
    assert_eq!(counts, [("1".to_string(), 50)]);
}

/// Repeat until success: a loop on a measurement result, with its variable kept in a
/// classical bit, is one circuit.
const UNTIL_ONE: &str = "fn test() -> (Bit, Int) {
    let mut done = false
    let mut tries = 0
    let q = |0>
    while not done {
        let t = |+>
        done = mv t.measure() == 1
    }
    q.x()
    let last = mv q.measure()
    // Classical work afterwards, on the values the loop left.
    if done { tries = 1 }
    (last, tries)
}";

#[test]
fn measurement_loops_are_circuits() {
    let (counts, circuit) = sampled(UNTIL_ONE, 200);
    assert!(circuit);
    assert_eq!(counts, [("(1, 1)".to_string(), 200)]);
}

#[test]
fn branches_that_measure_are_circuits() {
    // Whichever branch runs measures a fresh |1>, so `m` ends up 1 either way, and the
    // branch's variable decides the flip.
    let (counts, circuit) = sampled(
        "fn test() -> (Bit, Bit) {
            let coin = |+>
            let mut m: Bit = 0
            let q = |0>
            if mv coin.measure() == 1 {
                let a = |1>
                m = mv a.measure()
            } else {
                let b = |+>
                b.h()
                b.x()
                m = mv b.measure()
            }
            if m == 1 { q.x() }
            (m, mv q.measure())
        }",
        300,
    );
    assert!(circuit);
    assert_eq!(counts, [("11".to_string(), 300)]);
}

#[test]
fn sample_blocks_read_values_around_them() {
    let (out, _) = output(
        "fn main() {
            let flip = true
            let n = 3
            let d = sample 50 {
                let q = |0>
                if flip { q.x() }
                (mv q.measure(), n)
            }
            print(d)
            let nested = sample 4 {
                let inner = sample 10 { let q = |1>; mv q.measure() }
                inner.count(1)
            }
            print(nested)
        }",
    );
    assert_eq!(out, "(1, 3)  50  100.0%\n10  4  100.0%\n");
}

#[test]
fn sample_blocks_measure_returned_qubits() {
    let (out, _) = output(
        "fn main() {
            let d = sample 20 {
                let r = |00>
                r[1].x()
                mv r
            }
            print(d.most_likely(), d.count([0, 1]))
        }",
    );
    assert_eq!(out, "01 20\n");
}

/// ketrs restricted to one level, to test how the runtime chooses between them.
struct OneLevel {
    circuits: bool,
}

impl Backend for OneLevel {
    fn name(&self) -> &str {
        if self.circuits {
            "circuits-only"
        } else {
            "gates-only"
        }
    }

    fn session(&self, seed: u64) -> Option<Box<dyn noev_runtime::backend::Session>> {
        if self.circuits {
            None
        } else {
            Ketrs.session(seed)
        }
    }

    fn run_circuit(
        &self,
        circuit: &noev_runtime::backend::Circuit,
        shots: usize,
        seed: u64,
    ) -> Option<Result<noev_runtime::backend::Counts, noev_runtime::backend::BackendError>> {
        if self.circuits {
            Ketrs.run_circuit(circuit, shots, seed)
        } else {
            None
        }
    }
}

fn main_on(source: &str, backend: &dyn Backend) -> Result<String, RuntimeError> {
    let program = compile(source);
    let out = Buffer::default();
    run_main(
        &program,
        backend,
        3,
        Output {
            stdout: Box::new(out.clone()),
            stderr: Box::new(Buffer::default()),
        },
    )?;
    Ok(String::from_utf8(out.0.borrow().clone()).unwrap())
}

const BELL_MAIN: &str = "fn main() {
    let d = sample 2000 {
        let a = |+>
        let b = |0>
        ctrl a { b.x() }
        (mv a.measure(), mv b.measure())
    }
    print(d.count((0, 1)) + d.count((1, 0)), d.shots())
}";

#[test]
fn circuit_only_backends_take_circuits() {
    let out = main_on(BELL_MAIN, &OneLevel { circuits: true }).unwrap();
    assert_eq!(out, "0 2000\n");
    // A run that is not a circuit needs gate-level control.
    let error = main_on(
        "fn main() {
            let d = sample 10 {
                let q = |+>
                let k = Int(mv q.measure())
                let r = |0>
                r.ry(Float(k))
                mv r.measure()
            }
            print(d)
        }",
        &OneLevel { circuits: true },
    )
    .unwrap_err();
    assert!(error.message.contains("only runs circuits"), "{error:?}");
}

#[test]
fn gate_only_backends_run_circuits_shot_by_shot() {
    let out = main_on(BELL_MAIN, &OneLevel { circuits: false }).unwrap();
    assert_eq!(out, "0 2000\n");
}

/// Every circuit job of `source`'s `main`, as OpenQASM 3, read back by ketrs's own parser
/// and sampled: each must give the same counts as the circuit it was written from.
fn qasm_round_trip(source: &str) -> usize {
    let program = compile(source);
    let backend = noev_runtime::backend::Recorder::new(&Ketrs);
    run_main(
        &program,
        &backend,
        5,
        Output {
            stdout: Box::new(Buffer::default()),
            stderr: Box::new(Buffer::default()),
        },
    )
    .unwrap();
    let jobs = backend.jobs.borrow();
    for (circuit, shots) in jobs.iter() {
        let text = noev_runtime::qasm::to_qasm3(circuit, &[]);
        let parsed = ketrs::qasm::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        let mut rng = <rand::rngs::SmallRng as rand::SeedableRng>::seed_from_u64(9);
        let read_back: Vec<(u64, usize)> = ketrs::Statevector::zero(parsed.n_qubits())
            .sample_circuit(&parsed, *shots, &mut rng)
            .unwrap()
            .into_iter()
            .collect();
        let direct = Ketrs.run_circuit(circuit, *shots, 9).unwrap().unwrap();
        assert_eq!(read_back, direct, "{text}");
    }
    jobs.len()
}

#[test]
fn qasm_reads_back_as_the_same_circuit() {
    assert_eq!(
        qasm_round_trip(&format!(
            "{TELEPORT}\nfn main() {{ print(sample 1000 {{ test() }}) }}"
        )),
        1
    );
    let jobs = qasm_round_trip(
        "rev fn tee(q: mut Qubit) { q.t() }
        fn main() {
            let theta = 0.7
            let a = sample 500 {
                let r = |0+1>
                r[0].ry(theta).s()
                ctrl r[1] { r[0].x() }
                ctrl r[0], r[1] { r[2].rz(1.25) }
                adj tee(r[2])
                r[1].swap(r[2])
                mv r
            }
            let b = sample 300 {
                let q = |+>
                q.u(0.1, 0.2, 0.3)
                adj q.u(0.4, 0.5, 0.6)
                mv q.measure()
            }
            let c = sample 300 {
                let x = |+>
                let y = |+>
                let z = |0>
                let p = mv x.measure()
                let r = mv y.measure()
                if p != r or not (p == 1) { z.x() } else { z.h() }
                (p, r, mv z.measure())
            }
            let d = sample 100 {
                let mut done = false
                while not done {
                    let t = |+>
                    done = mv t.measure() == 1
                }
                let q = |0>
                if done { q.x() }
                mv q.measure()
            }
            print(a, b, c, d)
        }",
    );
    assert_eq!(jobs, 4);
}

#[test]
fn circuits_are_exported_without_simulating_them() {
    // 40 qubits: far beyond a statevector, but recording the circuit needs no simulation.
    let program = compile(
        "fn main() {
            let d = sample 1000 {
                let r = [|0>; 40]
                r[0].h()
                for i in 0..39 {
                    ctrl r[i] { r[i + 1].x() }
                }
                mv r
            }
            print(d.shots())
            print(d.count([0; 40]))
        }",
    );
    let exporter = noev_runtime::backend::Exporter::default();
    let error = run_main(
        &program,
        &exporter,
        0,
        Output {
            stdout: Box::new(Buffer::default()),
            stderr: Box::new(Buffer::default()),
        },
    )
    .unwrap_err();
    // `shots` is known; `count` needs the results.
    assert!(error.unknown, "{error:?}");
    let jobs = exporter.jobs.borrow();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        (jobs[0].0.qubits, jobs[0].0.clbits, jobs[0].1),
        (40, 40, 1000)
    );
}

const ENERGY_MAIN: &str = "fn prep(theta: Float) -> [Qubit; 2] {
    let r = |00>
    r[0].ry(theta)
    r[1].h().s()
    ctrl r[0] { r[1].x() }
    mv r
}
fn main() {
    let h = 0.5 * Z(0) * Z(1) - 0.8 * X(0) + 1.5 * Y(1) + 0.25 * I()
    print(<prep(0.9)| h |prep(0.9)>)
}";

#[test]
fn expectations_are_estimated_without_the_state() {
    let exact: f64 = main_on(ENERGY_MAIN, &Ketrs)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Run by run is slow without optimisations, so less precision is asked for.
    let rough = ENERGY_MAIN.replace("|prep(0.9)>)", "|prep(0.9)> within 0.03)");
    for circuits in [true, false] {
        let estimate: f64 = main_on(&rough, &OneLevel { circuits })
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            (estimate - exact).abs() < 0.15,
            "circuits: {circuits}, estimate {estimate}, exact {exact}"
        );
    }
}

#[test]
fn precision_sets_how_many_runs_an_estimate_takes() {
    let exact: f64 = main_on(ENERGY_MAIN, &Ketrs)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Within 0.005 takes hundreds of thousands of runs per group; as one circuit, each group
    // is a single simulation.
    let precise = ENERGY_MAIN.replace("|prep(0.9)>)", "|prep(0.9)> within 0.005)");
    let estimate: f64 = main_on(&precise, &OneLevel { circuits: true })
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        (estimate - exact).abs() < 0.025,
        "estimate {estimate}, exact {exact}"
    );
    let zero = ENERGY_MAIN.replace("|prep(0.9)>)", "|prep(0.9)> within 0.0)");
    let error = main_on(&zero, &OneLevel { circuits: true }).unwrap_err();
    assert!(error.message.contains("must be positive"), "{error:?}");
}

#[test]
fn expectation_circuits_are_exported() {
    let program = compile(ENERGY_MAIN);
    let exporter = noev_runtime::backend::Exporter::default();
    let error = run_main(
        &program,
        &exporter,
        0,
        Output {
            stdout: Box::new(Buffer::default()),
            stderr: Box::new(Buffer::default()),
        },
    )
    .unwrap_err();
    assert!(error.unknown, "{error:?}");
    // Two groups of terms measured together: Z0·Z1, and X0 with Y1.
    let jobs = exporter.jobs.borrow();
    assert_eq!(jobs.len(), 2, "{jobs:?}");
}
