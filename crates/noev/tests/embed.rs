//! End-to-end integration tests for the safe Rust embedding API (`spec/interop.md` §1, §2, §4, §5).

use std::io::Write;
use std::sync::{Arc, Mutex};

use noev::{
    Bit, C32, Complex, Error, HostContext, Observable, Output, Pauli, Program, Runtime, Value,
};

#[test]
fn call_quantum_functions_from_rust() {
    let program = Program::compile_source(
        r#"
fn ansatz(theta: Float) -> Qubit {
    let q = |0>
    q.ry(theta)
    mv q
}

fn energy(theta: Float) -> Float {
    <ansatz(theta)| Z(0) |ansatz(theta)>
}

fn bell_same_count(shots: Int) -> Int {
    let d = sample shots {
        let a = |+>
        let b = |0>
        ctrl a { b.x() }
        (mv a.measure(), mv b.measure())
    }
    d.count((0, 0)) + d.count((1, 1))
}
"#,
    )
    .expect("compiles");

    let mut rt = Runtime::new(&program).seed(42);
    let e0: f64 = rt.call("energy", (0.0,)).expect("energy(0)");
    let epi: f64 = rt
        .call("energy", (std::f64::consts::PI,))
        .expect("energy(pi)");
    let ehalf: f64 = rt
        .call("energy", (std::f64::consts::FRAC_PI_2,))
        .expect("energy(pi/2)");
    assert!((e0 - 1.0).abs() < 1e-12);
    assert!((epi + 1.0).abs() < 1e-12);
    assert!(ehalf.abs() < 1e-12);

    let same: i64 = rt.call("bell_same_count", (100i64,)).expect("bell");
    assert_eq!(same, 100);
}

#[test]
fn all_boundary_types_round_trip() {
    let program = Program::compile_source(
        r#"
struct Pair {
    label: String,
    value: Float,
}

extern fn host_echo_nums(a: Int, b: i8, c: u16, d: i32, e: u64, f: f32) -> (Int, u64)
extern fn host_echo_extra(b: Bool, bit: Bit, z: Complex, z32: c32, s: String, arr: [Float; 3]) -> (Bit, Complex, String, [Float; 3])
extern fn host_obs_and_struct(obs: Observable, p: Pair) -> (Observable, Pair)

fn test_nums(a: Int, b: i8, c: u16, d: i32, e: u64, f: f32) -> (Int, u64) {
    host_echo_nums(a, b, c, d, e, f)
}

fn test_extra(b: Bool, bit: Bit, z: Complex, z32: c32, s: String, arr: [Float; 3]) -> (Bit, Complex, String, [Float; 3]) {
    host_echo_extra(b, bit, z, z32, s, arr)
}

fn test_obs_and_struct(obs: Observable, p: Pair) -> (Observable, Pair) {
    host_obs_and_struct(obs, p)
}

fn test_unit() {
}

fn test_i64_sized(x: i64) -> i64 {
    x + 1
}
"#,
    )
    .expect("compiles");

    let mut rt = Runtime::new(&program)
        .host_fn(
            "host_echo_nums",
            |a: i64, b: i8, c: u16, d: i32, e: u64, f: f32| {
                assert_eq!(b, -7);
                assert_eq!(c, 500);
                assert_eq!(d, -100_000);
                assert!((f - 2.5).abs() < 1e-6);
                (a + 1, e + 2)
            },
        )
        .host_fn(
            "host_echo_extra",
            |b: bool, bit: Bit, z: Complex, z32: C32, s: String, arr: [f64; 3]| {
                assert!(b);
                assert!((z32.re - 1.5).abs() < 1e-6 && (z32.im + 0.5).abs() < 1e-6);
                (
                    Bit(!bit.0),
                    Complex::new(z.re * 2.0, z.im * 2.0),
                    format!("{s}!"),
                    [arr[2], arr[1], arr[0]],
                )
            },
        )
        .host_fn("host_obs_and_struct", |obs: Observable, p: Value| (obs, p));

    let (a, e): (i64, u64) = rt
        .call(
            "test_nums",
            (41i64, -7i8, 500u16, -100_000i32, 99u64, 2.5f32),
        )
        .expect("nums");
    assert_eq!((a, e), (42, 101));

    let (bit, z, s, arr): (Bit, Complex, String, [f64; 3]) = rt
        .call(
            "test_extra",
            (
                true,
                Bit(true),
                Complex::new(1.0, -2.0),
                C32::new(1.5, -0.5),
                "noev",
                [1.0, 2.0, 3.0],
            ),
        )
        .expect("extra");
    assert_eq!(bit, Bit(false));
    assert_eq!(z, Complex::new(2.0, -4.0));
    assert_eq!(s, "noev!");
    assert_eq!(arr, [3.0, 2.0, 1.0]);

    let obs = Observable {
        terms: vec![(0.5, vec![(0, Pauli::Z)])],
    };
    let pair = Value::Aggregate(vec![Value::Str("theta".into()), Value::Float(0.25)]);
    let (obs_out, pair_out): (Observable, Value) = rt
        .call("test_obs_and_struct", (obs.clone(), pair.clone()))
        .expect("obs+struct");
    assert_eq!(obs_out, obs);
    assert_eq!(pair_out, pair);

    let (): () = rt.call("test_unit", ()).expect("unit");
    let x: i64 = rt.call("test_i64_sized", (10i64,)).expect("i64");
    assert_eq!(x, 11);
}

#[test]
fn vqe_host_optimiser_matches_spec_sketch() {
    // Matches the exact VQE pattern in `spec/interop.md` §2.
    let program = Program::compile_source(
        r#"
extern fn minimize_step(energy: Float, theta: Float) -> Float

fn ansatz(theta: Float) -> Qubit {
    let q = |0>
    q.ry(theta)
    mv q
}

fn vqe() -> Float {
    let h = Z(0)
    let mut theta = 0.2
    for i in 0..40 {
        theta = minimize_step(<ansatz(theta)| h |ansatz(theta)>, theta)
    }
    <ansatz(theta)| h |ansatz(theta)>
}
"#,
    )
    .expect("compiles");

    struct StepOptimiser {
        prev: Option<(f64, f64)>,
        lr: f64,
        steps: usize,
    }
    impl StepOptimiser {
        fn step(&mut self, energy: f64, theta: f64) -> f64 {
            self.steps += 1;
            let next = match self.prev {
                None => theta + 0.1,
                Some((prev_e, prev_t)) => {
                    let dt = theta - prev_t;
                    if dt.abs() < 1e-12 {
                        theta
                    } else {
                        let grad = (energy - prev_e) / dt;
                        theta - self.lr * grad
                    }
                }
            };
            self.prev = Some((energy, theta));
            next
        }
    }

    let mut optimiser = StepOptimiser {
        prev: None,
        lr: 0.4,
        steps: 0,
    };

    let energy: f64 = {
        let mut runtime = Runtime::new(&program)
            .host_fn("minimize_step", |energy: f64, theta: f64| {
                optimiser.step(energy, theta)
            });
        runtime.call("vqe", ()).expect("vqe runs")
    };

    assert_eq!(optimiser.steps, 40);
    assert!(
        (energy + 1.0).abs() < 1e-6,
        "expected ground-state energy -1.0, got {energy}"
    );
}

#[test]
fn reentrant_host_callback_runs_quantum_code() {
    let program = Program::compile_source(
        r#"
extern fn optimize(start: Float) -> Float

fn ansatz(theta: Float) -> Qubit {
    let q = |0>
    q.ry(theta)
    mv q
}

fn energy(theta: Float) -> Float {
    <ansatz(theta)| Z(0) |ansatz(theta)>
}

fn run_vqe() -> (Float, Float) {
    let best_theta = optimize(0.3)
    (best_theta, energy(best_theta))
}
"#,
    )
    .expect("compiles");

    let mut eval_count = 0usize;
    let (best_theta, best_energy): (f64, f64) = {
        let mut runtime = Runtime::new(&program).host_fn_with_context(
            "optimize",
            |ctx: &mut HostContext<'_, '_>, start: f64| -> Result<f64, Error> {
                let mut theta = start;
                let eps = 0.05;
                for _ in 0..30 {
                    let e_plus: f64 = ctx.call("energy", (theta + eps,))?;
                    let e_minus: f64 = ctx.call("energy", (theta - eps,))?;
                    eval_count += 2;
                    let grad = (e_plus - e_minus) / (2.0 * eps);
                    theta -= 0.5 * grad;
                }
                Ok(theta)
            },
        );
        runtime.call("run_vqe", ()).expect("re-entrant vqe runs")
    };

    assert_eq!(eval_count, 60);
    assert!(
        (best_energy + 1.0).abs() < 1e-6,
        "expected energy -1.0 at theta={best_theta}, got {best_energy}"
    );
}

#[test]
fn reentrant_callback_reports_runtime_errors_to_host() {
    let program = Program::compile_source(
        r#"
extern fn try_call() -> Int

fn boom() -> Int {
    assert false
    1
}

fn entry() -> Int {
    try_call()
}
"#,
    )
    .expect("compiles");

    let mut rt = Runtime::new(&program).host_fn_with_context(
        "try_call",
        |ctx: &mut HostContext<'_, '_>| -> Result<i64, Error> {
            match ctx.call::<(), i64>("boom", ()) {
                Err(Error::Runtime(_)) => Ok(99),
                other => panic!("expected Error::Runtime, got {other:?}"),
            }
        },
    );

    let result: i64 = rt.call("entry", ()).expect("host recovered from error");
    assert_eq!(result, 99);
}

#[test]
fn reentrant_self_recursion_returns_error_not_panic() {
    let program = Program::compile_source(
        r#"
extern fn ping() -> Int

fn bounce() -> Int {
    ping()
}
"#,
    )
    .expect("compiles");

    let mut rt = Runtime::new(&program).host_fn_with_context(
        "ping",
        |ctx: &mut HostContext<'_, '_>| -> Result<i64, Error> { ctx.call("bounce", ()) },
    );

    let err = rt
        .call::<(), i64>("bounce", ())
        .expect_err("self-reentrant FnMut should fail cleanly");
    assert!(
        matches!(err, Error::Runtime(ref r) if r.message.contains("re-entrantly")),
        "unexpected error: {err:?}"
    );
}

#[test]
fn host_fn_inside_sample_and_expectation() {
    let program = Program::compile_source(
        r#"
extern fn angle() -> Float

fn prep() -> Qubit {
    let q = |0>
    q.ry(angle())
    mv q
}

fn from_expectation() -> Float {
    <prep()| Z(0) |prep()>
}

fn from_sample() -> Int {
    let d = sample 20 {
        let q = |0>
        q.ry(angle())
        mv q.measure()
    }
    d.count(1)
}
"#,
    )
    .expect("compiles");

    let mut rt = Runtime::new(&program)
        .seed(7)
        .host_fn("angle", || std::f64::consts::PI);

    let e: f64 = rt.call("from_expectation", ()).expect("expectation");
    assert!((e + 1.0).abs() < 1e-12);

    let ones: i64 = rt.call("from_sample", ()).expect("sample");
    assert_eq!(ones, 20);
}

#[test]
fn load_time_validation_errors() {
    let program = Program::compile_source(
        r#"
extern fn add(a: Float, b: Int) -> Float
fn run() -> Float { add(1.0, 2) }
"#,
    )
    .expect("compiles");

    // 1. Missing host implementation.
    let mut rt_missing = Runtime::new(&program);
    assert!(matches!(
        rt_missing.validate(),
        Err(Error::MissingHostFn(_))
    ));
    assert!(matches!(
        rt_missing.call::<(), f64>("run", ()),
        Err(Error::MissingHostFn(_))
    ));

    // 2. Wrong arity.
    let rt_arity = Runtime::new(&program).host_fn("add", |a: f64| a);
    assert!(matches!(
        rt_arity.validate(),
        Err(Error::SignatureMismatch(_))
    ));

    // 3. Wrong parameter type.
    let rt_param = Runtime::new(&program).host_fn("add", |a: i64, b: i64| (a + b) as f64);
    assert!(matches!(
        rt_param.validate(),
        Err(Error::SignatureMismatch(_))
    ));

    // 4. Wrong return type.
    let rt_ret = Runtime::new(&program).host_fn("add", |_a: f64, b: i64| b);
    assert!(matches!(
        rt_ret.validate(),
        Err(Error::SignatureMismatch(_))
    ));

    // 5. Registering a non-existent host fn name.
    let rt_unknown = Runtime::new(&program)
        .host_fn("add", |a: f64, b: i64| a + b as f64)
        .host_fn("typo", || 0i64);
    assert!(matches!(
        rt_unknown.validate(),
        Err(Error::UnknownHostFn(_))
    ));

    // 6. Registering a regular Noev fn as a host fn.
    let rt_not_extern = Runtime::new(&program)
        .host_fn("add", |a: f64, b: i64| a + b as f64)
        .host_fn("run", || 0.0f64);
    assert!(matches!(
        rt_not_extern.validate(),
        Err(Error::UnknownHostFn(_))
    ));
}

#[test]
fn boundary_type_and_checked_int_errors() {
    let program = Program::compile_source(
        r#"
fn takes_qubit(q: Qubit) { mv q.discard() }
fn returns_qubit() -> Qubit { |0> }
fn takes_ref(x: ref Int) -> Int { x }
fn huge_int() -> Int { 1 << 80 }
fn ok(x: Float) -> Float { x }
"#,
    )
    .expect("compiles");

    let mut rt = Runtime::new(&program);

    assert!(matches!(
        rt.call::<(Value,), ()>("takes_qubit", (Value::Unit,)),
        Err(Error::BoundaryType(_))
    ));
    assert!(matches!(
        rt.call::<(), Value>("returns_qubit", ()),
        Err(Error::BoundaryType(_))
    ));
    assert!(matches!(
        rt.call::<(i64,), i64>("takes_ref", (1,)),
        Err(Error::BoundaryType(_))
    ));
    assert!(matches!(
        rt.call::<(), ()>("no_such_fn", ()),
        Err(Error::UnknownFunction(_))
    ));
    assert!(matches!(
        rt.call::<(i64,), f64>("ok", (1,)),
        Err(Error::SignatureMismatch(_))
    ));
    assert!(matches!(
        rt.call::<(f64,), i64>("ok", (1.0,)),
        Err(Error::SignatureMismatch(_))
    ));
    assert!(matches!(
        rt.call::<(), i64>("huge_int", ()),
        Err(Error::Conversion(msg)) if msg.contains("64-bit")
    ));
}

#[test]
fn compile_time_extern_declaration_checks() {
    for bad in [
        "extern fn bad(q: Qubit)",
        "extern fn bad() -> Qubit",
        "extern fn bad(x: ref Int) -> Int",
        "extern fn bad(x: mut Int)",
        "extern rev fn bad(x: Float) -> Float",
        "extern unsafe fn bad(x: Float) -> Float",
        "extern fn bad(x: Float) -> Float { x }",
        "extern \"Fortran\" { fn erf(x: Float) -> Float }",
    ] {
        let res = Program::compile_source(bad);
        assert!(
            matches!(res, Err(Error::Compile(_))),
            "expected compile error for `{bad}`, got {res:?}"
        );
    }
}

#[test]
fn circuit_classification_with_extern_calls() {
    // Spec/interop.md §5:
    // - An extern call not using a measurement result keeps the run a circuit.
    // - An extern call using a measurement result before the last quantum op makes it NOT a circuit.
    // - An extern call using a measurement result after the last quantum op must not receive a
    //   bogus placeholder during circuit recording.
    let program = Program::compile_source(
        r#"
extern fn angle() -> Float
extern fn decide(b: Bit) -> Float
extern fn observe(b: Bit) -> Int

fn circuit_with_host() -> Bit {
    let q = |0>
    q.ry(angle())
    mv q.measure()
}

fn non_circuit_with_host() -> Bit {
    let a = |+>
    let b = |0>
    let m = mv a.measure()
    let theta = decide(m)
    b.ry(theta)
    mv b.measure()
}

fn after_last_quantum_op() -> Int {
    let d = sample 8 {
        let q = |1>
        let b = mv q.measure()
        observe(b)
    }
    d.count(1)
}
"#,
    )
    .expect("compiles");

    let is_sampled = |name: &str| {
        program
            .ir()
            .functions
            .iter()
            .find(|f| f.name == name)
            .expect("function exists")
            .sampled
    };
    assert!(is_sampled("circuit_with_host"));
    assert!(!is_sampled("non_circuit_with_host"));

    let mut seen_bits = Vec::new();
    let count: i64 = {
        let mut rt = Runtime::new(&program)
            .seed(1)
            .host_fn("angle", || 0.0f64)
            .host_fn("decide", |b: Bit| if b.0 { 1.0 } else { 0.0 })
            .host_fn("observe", |b: Bit| {
                seen_bits.push(b.0);
                i64::from(b.0)
            });
        rt.call("after_last_quantum_op", ()).expect("runs")
    };
    assert_eq!(count, 8);
    assert!(!seen_bits.is_empty());
    assert!(
        seen_bits.iter().all(|&b| b),
        "host must never see a placeholder 0 bit for |1>; saw {seen_bits:?}"
    );
}

#[test]
fn compile_file_and_capture_output() {
    let dir = std::env::temp_dir().join(format!("noev_embed_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("prog.nv");
    std::fs::write(
        &path,
        r#"
extern fn notify(n: Int) -> Int

fn greet(x: Int) -> Int {
    print("hello", x)
    notify(x)
}
"#,
    )
    .unwrap();

    let program = Program::compile_file(&path).expect("compile_file");
    let _ = std::fs::remove_dir_all(&dir);

    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let stdout_buf = Arc::new(Mutex::new(Vec::new()));
    let output = Output {
        stdout: Box::new(SharedBuf(stdout_buf.clone())),
        stderr: Box::new(Vec::new()),
    };

    let mut rt = Runtime::new(&program)
        .output(output)
        .host_fn("notify", |n: i64| n * 2);
    let out: i64 = rt.call("greet", (21i64,)).expect("greet");
    assert_eq!(out, 42);
    assert_eq!(
        String::from_utf8(stdout_buf.lock().unwrap().clone()).unwrap(),
        "hello 21\n"
    );
}
