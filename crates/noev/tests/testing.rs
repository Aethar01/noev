//! End-to-end tests for the built-in Noev test system (`noev test`, `spec/testing.md`):
//! 1. Single-file `test fn`, `test(fail)`, `test(fail = "…")`, `assert cond, "msg"` (with lazy
//!    evaluation), `--list`, `--filter`, `--seed`, `--nocapture`, and failure reporting across
//!    interpreter, Cranelift (`--codegen=cranelift`), and LLVM (`--codegen=llvm`).
//! 2. Package discovery: unit `test fn`s in `src/` (accessing private items) plus integration
//!    tests in `tests/*.nv` importing the package library.
//! 3. Directory discovery (`noev test dir/` without `noev.toml`).
//! 4. Compile-time errors for invalid `test fn` signatures and non-`String` `assert` messages.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn noev_bin() -> &'static str {
    env!("CARGO_BIN_EXE_noev")
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("noev_testing_{name}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn ensure_runtime_lib() {
    static BUILT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-q", "-p", "noev-runtime"])
            .current_dir(repo_root())
            .status()
            .expect("cargo build -p noev-runtime");
        assert!(status.success(), "cargo build -p noev-runtime failed");
    });
}

#[test]
fn single_file_test_suite_interpreter_cranelift_and_llvm() {
    ensure_runtime_lib();
    let dir = temp_dir("single_file");
    let file = dir.join("suite.nv");
    fs::write(
        &file,
        r#"
// `test` remains a normal identifier outside `test fn`.
fn helper(test: Int) -> Int {
    test + 1
}

test fn arithmetic_and_assert_msg() {
    let a = helper(41)
    assert a == 42, "expected 42, got {a}"
    // Lazy message evaluation: dividing by zero in the message would fail if evaluated eagerly.
    let z = 0
    assert true, "unreachable {1 / z}"
}

test fn quantum_bell_state() {
    let counts = sample 40 {
        let mut a = |0>
        let mut b = |0>
        h(mut a)
        ctrl a { x(mut b) }
        (mv a.measure(), mv b.measure())
    }
    assert counts.shots() == 40
    assert counts.count((0, 1)) == 0, "impossible Bell outcome 01"
    assert counts.count((1, 0)) == 0, "impossible Bell outcome 10"
}

test(fail) fn expected_any_failure() {
    let mut q = |0>
    x(mut q)
    unsafe { mv q.release() }
}

test(fail = "custom boom: 7") fn expected_substring_failure() {
    let v = 7
    assert v == 8, "custom boom: {v}"
}
"#,
    )
    .unwrap();

    // 1. `--list`
    let list = Command::new(noev_bin())
        .args(["test", "--list"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(list.status.success());
    let listed = String::from_utf8_lossy(&list.stdout);
    assert_eq!(
        listed.lines().collect::<Vec<_>>(),
        vec![
            "arithmetic_and_assert_msg",
            "quantum_bell_state",
            "expected_any_failure",
            "expected_substring_failure",
        ]
    );

    // 2. `--filter`
    let filt = Command::new(noev_bin())
        .args(["test", "--seed", "42"])
        .arg(&file)
        .arg("bell")
        .output()
        .unwrap();
    assert!(
        filt.status.success(),
        "filter failed:\n{}",
        String::from_utf8_lossy(&filt.stderr)
    );
    let filt_out = String::from_utf8_lossy(&filt.stdout);
    assert!(filt_out.contains("running 1 test"), "{filt_out}");
    assert!(
        filt_out.contains("test quantum_bell_state ... ok"),
        "{filt_out}"
    );
    assert!(filt_out.contains("1 passed; 0 failed"), "{filt_out}");

    // 3. Full run across interpreter, Cranelift, and LLVM.
    for mode in [
        vec!["test", "--seed", "99"],
        vec!["test", "--seed", "99", "--codegen=cranelift"],
        vec!["test", "--seed", "99", "--codegen=llvm"],
    ] {
        let out = Command::new(noev_bin())
            .args(&mode)
            .arg(&file)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "mode {mode:?} failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(stdout.contains("running 4 tests"), "{mode:?}:\n{stdout}");
        assert!(
            stdout.contains("test result: ok. 4 passed; 0 failed"),
            "{mode:?}:\n{stdout}"
        );
    }
}

#[test]
fn failing_tests_capture_output_and_report_failures() {
    ensure_runtime_lib();
    let dir = temp_dir("failures");
    let file = dir.join("fail_cases.nv");
    fs::write(
        &file,
        r#"
test fn ok_with_hidden_print() {
    print("hidden_passing_line")
    assert true
}

test fn fails_with_message() {
    print("captured_failing_line")
    assert 2 + 2 == 5, "math broke"
}

test(fail = "expected needle") fn fails_wrong_substring() {
    assert false, "different error"
}

test(fail) fn fails_because_it_passed() {
    assert true
}
"#,
    )
    .unwrap();

    for mode in [
        vec!["test", "--seed", "7"],
        vec!["test", "--seed", "7", "--compile"],
    ] {
        let out = Command::new(noev_bin())
            .args(&mode)
            .arg(&file)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{mode:?} should fail");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let combined = format!("{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
        assert!(
            !stdout.contains("hidden_passing_line"),
            "passing print should be captured ({mode:?}):\n{stdout}"
        );
        assert!(
            stdout.contains("captured_failing_line"),
            "failing print should be shown ({mode:?}):\n{stdout}"
        );
        assert!(combined.contains("math broke"), "{mode:?}:\n{combined}");
        assert!(
            combined.contains("expected failure containing `expected needle`"),
            "{mode:?}:\n{combined}"
        );
        assert!(
            combined.contains("expected to fail, but passed"),
            "{mode:?}:\n{combined}"
        );
        assert!(
            stdout.contains("test result: FAILED. 1 passed; 3 failed"),
            "{mode:?}:\n{stdout}"
        );
    }

    // `--nocapture` shows passing test output live.
    let nocap = Command::new(noev_bin())
        .args(["test", "--nocapture"])
        .arg(&file)
        .arg("ok_with_hidden_print")
        .output()
        .unwrap();
    assert!(nocap.status.success());
    assert!(
        String::from_utf8_lossy(&nocap.stdout).contains("hidden_passing_line"),
        "expected live output with --nocapture"
    );
}

#[test]
fn package_unit_and_integration_tests() {
    ensure_runtime_lib();
    let dir = temp_dir("pkg");
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::create_dir_all(dir.join("tests")).unwrap();

    fs::write(
        dir.join("noev.toml"),
        r#"
[package]
name = "qmath"
"#,
    )
    .unwrap();

    fs::write(
        dir.join("src/lib.nv"),
        r#"
fn secret_double(x: Int) -> Int {
    x * 2
}

pub fn quadruple(x: Int) -> Int {
    secret_double(secret_double(x))
}

test fn unit_sees_private_helper() {
    assert secret_double(21) == 42
}
"#,
    )
    .unwrap();

    fs::write(
        dir.join("src/gates.nv"),
        r#"
pub rev fn flip_pair(a: mut Qubit, b: mut Qubit) {
    x(mut a)
    x(mut b)
}

test fn unit_in_submodule() {
    let mut a = |0>
    let mut b = |0>
    flip_pair(mut a, mut b)
    assert mv a.measure() == 1
    assert mv b.measure() == 1
}
"#,
    )
    .unwrap();

    fs::write(
        dir.join("tests/api.nv"),
        r#"
use qmath::quadruple
use qmath::gates::flip_pair

test fn integration_uses_public_api() {
    assert quadruple(3) == 12
    let mut q0 = |0>
    let mut q1 = |1>
    flip_pair(mut q0, mut q1)
    assert mv q0.measure() == 1
    assert mv q1.measure() == 0
}
"#,
    )
    .unwrap();

    for mode in [vec!["test"], vec!["test", "--compile"]] {
        let out = Command::new(noev_bin())
            .args(&mode)
            .arg(&dir)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "package test {mode:?} failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stdout.contains("test unit_sees_private_helper ... ok"),
            "{stdout}"
        );
        assert!(
            stdout.contains("test gates::unit_in_submodule ... ok"),
            "{stdout}"
        );
        assert!(
            stdout.contains("test api::integration_uses_public_api ... ok"),
            "{stdout}"
        );
        assert!(
            stdout.contains("test result: ok. 3 passed; 0 failed"),
            "{stdout}"
        );
    }
}

#[test]
fn invalid_test_signatures_are_rejected() {
    let dir = temp_dir("bad_sigs");
    let bad = dir.join("bad.nv");
    fs::write(
        &bad,
        r#"
test fn has_param(x: Int) {}
test fn has_ret() -> Int { 1 }
test fn is_generic<T>() {}
test rev fn is_rev() {}
test fn bad_assert_msg() {
    assert true, 123
}
"#,
    )
    .unwrap();

    let out = Command::new(noev_bin())
        .args(["check"])
        .arg(&bad)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot take parameters"), "stderr:\n{err}");
    assert!(err.contains("must return `()`"), "stderr:\n{err}");
    assert!(
        err.contains("cannot have generic parameters"),
        "stderr:\n{err}"
    );
    assert!(err.contains("cannot be `rev`"), "stderr:\n{err}");
    assert!(err.contains("String"), "stderr:\n{err}");
}
