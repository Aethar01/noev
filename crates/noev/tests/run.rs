//! Runs the conformance suite in `tests/run` via `noev test` (interpreter), plus smoke checks
//! for `noev run` stdout/stderr and inferred `Copy` bound rejection.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/run")
}

#[test]
fn run() {
    let output = Command::new(env!("CARGO_BIN_EXE_noev"))
        .args(["test", "--seed", "1"])
        .arg(run_dir())
        .output()
        .expect("noev test runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "noev test tests/run failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("test result: ok.") && stdout.contains("0 failed"),
        "unexpected test summary:\n{stdout}"
    );
}

#[test]
fn noev_run_stdout_and_stderr() {
    let dir = std::env::temp_dir().join(format!("noev_run_smoke_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let nv = dir.join("hello.nv");
    fs::write(
        &nv,
        r#"
fn main() {
    print("hello", 42)
    eprint("err-line")
}
"#,
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_noev"))
        .args(["run"])
        .arg(&nv)
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello 42\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "err-line\n");
}

#[test]
fn inferred_copy_bound_rejects_qubit() {
    let dir = std::env::temp_dir().join(format!("noev_inferred_copy_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let nv = dir.join("copy_fail.nv");
    fs::write(
        &nv,
        r#"
fn twice<T>(x: T) -> (T, T) { (x, x) }

fn main() {
    let (a, b) = twice(|0>)
    mv a.discard()
    mv b.discard()
}
"#,
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_noev"))
        .args(["check"])
        .arg(&nv)
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("`twice` needs `T: Copy`, but here `T` is `Qubit`"),
        "stderr:\n{stderr}"
    );
}
