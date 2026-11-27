//! Integration tests for inline OpenQASM (`spec/interop.md` §4):
//! OpenQASM 3 verbatim export, hardware-only error on ketrs, QIR rejection, and IR emission.

use std::path::PathBuf;
use std::process::Command;

fn noev_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_noev"))
}

fn write_temp(name: &str, src: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("noev_qasm_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, src).unwrap();
    path
}

#[test]
fn export_emits_hardware_qasm_verbatim() {
    let file = write_temp(
        "hardware_export.nv",
        r#"
fn main() {
    let _ = sample 100 {
        let a = |0>
        let b = |0>
        let theta = 1.5
        unsafe {
            qasm("sx {a}; rz({theta}) {a};\necr {a}, {b};\ndelay[100ns] {a};\nbarrier {a}, {b};")
        }
        (mv a.measure(), mv b.measure())
    }
}
"#,
    );
    let out = Command::new(noev_bin())
        .args(["export"])
        .arg(&file)
        .output()
        .expect("run noev export");
    assert!(
        out.status.success(),
        "noev export failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let qasm = String::from_utf8_lossy(&out.stdout);
    assert!(qasm.contains("sx q[0]; rz(1.5) q[0];"), "got:\n{qasm}");
    assert!(qasm.contains("ecr q[0], q[1];"), "got:\n{qasm}");
    assert!(qasm.contains("delay[100ns] q[0];"), "got:\n{qasm}");
    assert!(qasm.contains("barrier q[0], q[1];"), "got:\n{qasm}");
    let _ = std::fs::remove_file(&file);
}

#[cfg(feature = "ketrs")]
#[test]
fn hardware_only_qasm_errors_cleanly_on_ketrs() {
    let file = write_temp(
        "hardware_sim_err.nv",
        r#"
fn main() {
    let q = |0>
    unsafe { qasm("delay[100ns] {q};") }
    mv q.discard()
}
"#,
    );
    let out = Command::new(noev_bin())
        .args(["run"])
        .arg(&file)
        .output()
        .expect("run noev run");
    assert!(
        !out.status.success(),
        "expected error simulating hardware-only delay directive"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot simulate inline OpenQASM on ketrs"),
        "expected clear ketrs error, got:\n{stderr}"
    );
    let _ = std::fs::remove_file(&file);
}

#[test]
fn qir_export_rejects_inline_qasm() {
    let file = write_temp(
        "qir_reject.nv",
        r#"
fn main() {
    let _ = sample 10 {
        let q = |0>
        unsafe { qasm("sx {q};") }
        mv q.measure()
    }
}
"#,
    );
    let out = Command::new(noev_bin())
        .args(["build", "--emit=qir"])
        .arg(&file)
        .output()
        .expect("run noev build --emit=qir");
    assert!(
        !out.status.success(),
        "expected QIR export to reject inline OpenQASM"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot be exported to QIR"),
        "expected clear QIR error, got:\n{stderr}"
    );
    let _ = std::fs::remove_file(&file);
}

#[test]
fn emit_ir_displays_qasm_stmt() {
    let file = write_temp(
        "emit_ir.nv",
        r#"
fn main() {
    let q = |0>
    let theta = 0.25
    unsafe { qasm("rz({theta}) {q};") }
    mv q.discard()
}
"#,
    );
    let out = Command::new(noev_bin())
        .args(["build", "--emit=ir"])
        .arg(&file)
        .output()
        .expect("run noev build --emit=ir");
    assert!(
        out.status.success(),
        "emit=ir failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ir = String::from_utf8_lossy(&out.stdout);
    assert!(ir.contains("qasm ["), "expected qasm stmt in IR:\n{ir}");
    let _ = std::fs::remove_file(&file);
}

#[test]
fn three_qubit_unitary_exports_to_qasm_and_qir() {
    let file = write_temp(
        "unitary3_export.nv",
        r#"
fn main() {
    let mut ccx = [[0.0 + 0i; 8]; 8]
    for i in 0..8 {
        ccx[i][i] = 1.0 + 0i
    }
    ccx[6][6] = 0.0 + 0i
    ccx[7][7] = 0.0 + 0i
    ccx[6][7] = 1.0 + 0i
    ccx[7][6] = 1.0 + 0i
    let _ = sample 10 {
        let r = |110>
        r.unitary(ccx)
        mv r.measure()
    }
}
"#,
    );
    let qasm_out = Command::new(noev_bin())
        .args(["export"])
        .arg(&file)
        .output()
        .expect("run noev export");
    assert!(
        qasm_out.status.success(),
        "noev export failed: {}",
        String::from_utf8_lossy(&qasm_out.stderr)
    );
    let qasm = String::from_utf8_lossy(&qasm_out.stdout);
    assert!(qasm.contains("qubit[3] q;"), "got:\n{qasm}");
    assert!(
        qasm.contains("ctrl @ x "),
        "expected decomposed CNOTs:\n{qasm}"
    );

    let qir_out = Command::new(noev_bin())
        .args(["build", "--emit=qir"])
        .arg(&file)
        .output()
        .expect("run noev build --emit=qir");
    assert!(
        qir_out.status.success(),
        "noev build --emit=qir failed: {}",
        String::from_utf8_lossy(&qir_out.stderr)
    );
    let qir = String::from_utf8_lossy(&qir_out.stdout);
    assert!(qir.contains("__quantum__qis__cnot__body"), "got:\n{qir}");
    let _ = std::fs::remove_file(&file);
}
