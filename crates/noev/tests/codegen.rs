//! Compiles and runs the entire `tests/run/` conformance suite with Cranelift via
//! `noev test --codegen=cranelift`, and verifies `--emit=object` produces a valid native object.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/run")
}

#[test]
fn compiled_matches_interpreter() {
    let output = Command::new(env!("CARGO_BIN_EXE_noev"))
        .args(["test", "--codegen=cranelift", "--seed", "1"])
        .arg(run_dir())
        .output()
        .expect("noev test --codegen=cranelift runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "noev test --codegen=cranelift tests/run failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("test result: ok.") && stdout.contains("0 failed"),
        "unexpected test summary:\n{stdout}"
    );
}

#[test]
fn emit_object_produces_native_object() {
    let tmp_dir = std::env::temp_dir().join(format!("noev-obj-test-{}", std::process::id()));
    fs::create_dir_all(&tmp_dir).expect("temp dir");
    let obj_path = tmp_dir.join("out.o");
    let src = run_dir().join("gates.nv");

    let build = Command::new(env!("CARGO_BIN_EXE_noev"))
        .args(["build", "--emit=object", "-o"])
        .arg(&obj_path)
        .arg(&src)
        .output()
        .expect("noev build --emit=object runs");
    assert!(
        build.status.success(),
        "emit=object failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let bytes = fs::read(&obj_path).expect("object file written");
    let _ = fs::remove_dir_all(&tmp_dir);

    assert!(bytes.len() > 64, "object file unexpectedly small");
    #[cfg(target_os = "linux")]
    assert_eq!(&bytes[0..4], b"\x7fELF", "expected ELF header");
}
