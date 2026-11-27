//! Tests the LLVM native code generator (`noev build --codegen=llvm` / `noev test --codegen=llvm`):
//! 1. Compiles and runs the entire `tests/run/` conformance suite with LLVM (`-O2`) via
//!    `noev test --codegen=llvm`.
//! 2. Verifies `--codegen=llvm --qir-runtime --emit=object` links against a mock C QIR
//!    runtime and executes end-to-end.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/run")
}

#[test]
fn llvm_compiled_matches_interpreter() {
    let output = Command::new(env!("CARGO_BIN_EXE_noev"))
        .args(["test", "--codegen=llvm", "--seed", "1"])
        .arg(run_dir())
        .output()
        .expect("noev test --codegen=llvm runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "noev test --codegen=llvm tests/run failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("test result: ok.") && stdout.contains("0 failed"),
        "unexpected test summary:\n{stdout}"
    );
}

#[test]
fn llvm_qir_runtime_links_and_runs_with_mock_qir_runtime() {
    let tmp_dir = std::env::temp_dir().join(format!("noev-qir-rt-test-{}", std::process::id()));
    fs::create_dir_all(&tmp_dir).expect("temp dir");

    // A Noev program exercising QIR runtime calls: allocate, H, ctrl X, measure, reset, release.
    let nv_path = tmp_dir.join("qir_prog.nv");
    fs::write(
        &nv_path,
        "fn main() {\n\
            let a = |+>\n\
            let b = |0>\n\
            ctrl a { b.x() }\n\
            let m0 = mv a.measure()\n\
            let m1 = mv b.measure()\n\
            assert m0 == m1\n\
         }\n",
    )
    .expect("write nv");

    // Check `--emit=llvm-ir --qir-runtime` emits standard QIR runtime calls.
    let ir_out = Command::new(env!("CARGO_BIN_EXE_noev"))
        .args(["build", "--codegen=llvm", "--qir-runtime", "--emit=llvm-ir"])
        .arg(&nv_path)
        .output()
        .expect("emit=llvm-ir runs");
    assert!(
        ir_out.status.success(),
        "emit=llvm-ir failed:\n{}",
        String::from_utf8_lossy(&ir_out.stderr)
    );
    let ir = String::from_utf8(ir_out.stdout).expect("utf8");
    assert!(ir.contains("@__quantum__rt__initialize("));
    assert!(ir.contains("@__quantum__rt__qubit_allocate("));
    assert!(ir.contains("@__quantum__qis__h__body("));
    assert!(ir.contains("@__quantum__qis__x__body("));
    assert!(ir.contains("@__quantum__qis__m__body("));
    assert!(ir.contains("@__quantum__rt__read_result("));

    // Compile to native `.o` targeting the QIR runtime ABI.
    let obj_path = tmp_dir.join("qir_prog.o");
    let build = Command::new(env!("CARGO_BIN_EXE_noev"))
        .args([
            "build",
            "--codegen=llvm",
            "--qir-runtime",
            "--emit=object",
            "-o",
        ])
        .arg(&obj_path)
        .arg(&nv_path)
        .output()
        .expect("emit=object --qir-runtime runs");
    assert!(
        build.status.success(),
        "emit=object --qir-runtime failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    // Link against a tiny standalone C QIR runtime shim and run the resulting native binary!
    let shim_c = tmp_dir.join("qir_shim.c");
    fs::write(
        &shim_c,
        r#"
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>

static uintptr_t next_q = 1;
static int ctrl_depth = 0;

void __quantum__rt__initialize(void *args) { (void)args; printf("init\n"); }
void *__quantum__rt__qubit_allocate(void) {
    uintptr_t q = next_q++;
    printf("alloc %zu\n", (size_t)q);
    return (void *)q;
}
void __quantum__rt__qubit_release(void *q) { printf("release %zu\n", (size_t)(uintptr_t)q); }
void __quantum__qis__h__body(void *q) { printf("h %zu\n", (size_t)(uintptr_t)q); }
void __quantum__qis__x__body(void *q) { printf("x %zu ctrl=%d\n", (size_t)(uintptr_t)q, ctrl_depth); }
void __quantum__rt__ctrl_push(void **qs, int64_t n) { (void)qs; ctrl_depth += (int)n; }
void __quantum__rt__ctrl_pop(void) { ctrl_depth = 0; }
void *__quantum__qis__m__body(void *q) {
    printf("m %zu\n", (size_t)(uintptr_t)q);
    return (void *)1;
}
bool __quantum__rt__read_result(void *r) { return r != NULL; }
"#,
    )
    .expect("write shim");

    let bin_path = tmp_dir.join("qir_prog_bin");
    let link = Command::new(std::env::var("CC").unwrap_or_else(|_| "cc".into()))
        .arg(&obj_path)
        .arg(&shim_c)
        .arg("-o")
        .arg(&bin_path)
        .output()
        .expect("link with QIR shim");
    assert!(
        link.status.success(),
        "linking QIR object with shim failed:\n{}",
        String::from_utf8_lossy(&link.stderr)
    );

    let run = Command::new(&bin_path).output().expect("run QIR binary");
    let _ = fs::remove_dir_all(&tmp_dir);

    assert!(
        run.status.success(),
        "QIR binary exited with {:?}:\n{}",
        run.status,
        String::from_utf8_lossy(&run.stderr)
    );
    let trace = String::from_utf8_lossy(&run.stdout);
    assert_eq!(trace, "init\nalloc 1\nh 1\nalloc 2\nx 2 ctrl=1\nm 1\nm 2\n");
}
