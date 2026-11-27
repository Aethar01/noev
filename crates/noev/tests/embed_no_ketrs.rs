//! Rust embedding tests that do not require the `ketrs` feature (`--no-default-features`).

use noev::{Bit, Error, Program, Runtime};
use noev_runtime::backend::Exporter;

#[test]
fn classical_and_host_fn_without_ketrs() {
    let program = Program::compile_source(
        r#"
extern fn scale(x: Float, k: Int) -> Float

fn compute(x: Float) -> (Float, Bit) {
    let y = scale(x, 3)
    (y + 1.0, 1)
}
"#,
    )
    .expect("compiles");

    let backend = Exporter::default();
    let mut rt =
        Runtime::with_backend(&program, &backend).host_fn("scale", |x: f64, k: i64| x * (k as f64));

    let (val, bit): (f64, Bit) = rt.call("compute", (4.0,)).expect("runs");
    assert!((val - 13.0).abs() < 1e-12);
    assert_eq!(bit, Bit(true));
}

#[test]
fn validate_catches_missing_host_without_ketrs() {
    let program =
        Program::compile_source("extern fn missing(x: Int) -> Int\nfn f() -> Int { missing(1) }")
            .expect("compiles");
    let backend = Exporter::default();
    let rt = Runtime::with_backend(&program, &backend);
    assert!(matches!(rt.validate(), Err(Error::MissingHostFn(_))));
}
