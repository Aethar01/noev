//! Integration tests for QIR 1.0 export (`noev build --emit=qir` and `noev export --format=qir`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn root_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn emit_qir(rel_path: &str) -> String {
    let path = root_dir().join(rel_path);
    let output = Command::new(env!("CARGO_BIN_EXE_noev"))
        .args(["build", "--emit=qir"])
        .arg(&path)
        .output()
        .expect("noev build --emit=qir runs");
    assert!(
        output.status.success(),
        "noev build --emit=qir {} failed:\n{}",
        rel_path,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("valid UTF-8 QIR")
}

#[test]
fn base_profile_bell_example() {
    let ir = emit_qir("examples/bell.nv");
    assert!(ir.contains("%Qubit = type opaque"));
    assert!(ir.contains("%Result = type opaque"));
    assert!(ir.contains("define void @main() #0"));
    assert!(ir.contains("\"entry_point\""));
    assert!(ir.contains("\"qir_profiles\"=\"base_profile\""));
    assert!(ir.contains("\"required_num_qubits\"=\"2\""));
    assert!(ir.contains("\"required_num_results\"=\"2\""));
    assert!(ir.contains("@__quantum__qis__h__body(%Qubit* null)"));
    assert!(ir.contains("@__quantum__qis__cnot__body("));
    assert!(ir.contains("@__quantum__qis__mz__body("));
    assert!(ir.contains("@__quantum__rt__array_record_output(i64 2, i8* null)"));
    assert!(ir.contains("!{i32 1, !\"qir_major_version\", i32 1}"));
    assert!(!ir.contains("adaptive_profile"));
}

#[test]
fn base_profile_qft_example() {
    let ir = emit_qir("examples/qft.nv");
    assert!(ir.contains("\"qir_profiles\"=\"base_profile\""));
    assert!(ir.contains("\"required_num_qubits\"=\"16\""));
    assert!(ir.contains("\"required_num_results\"=\"16\""));
    assert!(ir.contains("@__quantum__qis__h__body("));
    assert!(ir.contains("@__quantum__qis__rz__body("));
    assert!(ir.contains("@__quantum__qis__cnot__body("));
    assert!(ir.contains("@__quantum__rt__array_record_output(i64 16, i8* null)"));
}

#[test]
fn adaptive_profile_teleport_and_feedback() {
    let teleport = emit_qir("examples/teleport.nv");
    assert!(
        teleport.contains("\"qir_profiles\"=\"adaptive_profile\""),
        "expected adaptive_profile for teleport:\n{teleport}"
    );
    assert!(teleport.contains("@__quantum__rt__read_result("));
    assert!(teleport.contains("br i1 "));

    let feedback = emit_qir("tests/run/feedback.nv");
    assert!(
        feedback.contains("\"qir_profiles\"=\"adaptive_profile\""),
        "expected adaptive_profile for feedback:\n{feedback}"
    );
    assert!(feedback.contains("while.cond."));
    assert!(feedback.contains("!{i32 1, !\"backwards_branching\", i1 true}"));
}

#[test]
fn emit_qir_to_file_and_export_format_qir() {
    let tmp = std::env::temp_dir().join(format!("noev-qir-test-{}", std::process::id()));
    fs::create_dir_all(&tmp).expect("temp dir");

    // `noev build --emit=qir -o out.ll`
    let ll_file = tmp.join("bell.ll");
    let build = Command::new(env!("CARGO_BIN_EXE_noev"))
        .args(["build", "--emit=qir", "-o"])
        .arg(&ll_file)
        .arg(root_dir().join("examples/bell.nv"))
        .output()
        .expect("noev build --emit=qir -o runs");
    assert!(build.status.success());
    let file_ir = fs::read_to_string(&ll_file).expect("read .ll");
    assert!(file_ir.contains("\"qir_profiles\"=\"base_profile\""));

    // `noev export --format=qir -o dir/`
    let export_dir = tmp.join("circuits");
    let export = Command::new(env!("CARGO_BIN_EXE_noev"))
        .args(["export", "--format=qir", "-o"])
        .arg(&export_dir)
        .arg(root_dir().join("examples/bell.nv"))
        .output()
        .expect("noev export --format=qir runs");
    assert!(export.status.success());
    let c1 = fs::read_to_string(export_dir.join("circuit-1.ll")).expect("read circuit-1.ll");
    assert!(c1.contains("define void @circuit_1() #0"));
    assert!(c1.contains("\"qir_profiles\"=\"base_profile\""));

    let _ = fs::remove_dir_all(&tmp);
}
