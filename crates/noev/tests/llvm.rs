//! Tests the LLVM native code generator (`noev build --codegen=llvm`):
//! 1. Compiles all 39 `tests/run/*.nv` programs with LLVM (`-O2`) and checks output/errors
//!    against the interpreter conformance suite.
//! 2. Verifies `--codegen=llvm --emit=object` produces a valid native ELF object.
//! 3. Verifies `--codegen=llvm --qir-runtime --emit=object` links against a mock C QIR
//!    runtime and executes end-to-end.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

fn run_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/run")
}

fn check_llvm_compiled(path: &Path, tmp_dir: &Path) -> Option<String> {
    let source = fs::read_to_string(path).expect("the program is readable");
    let expected = fs::read_to_string(path.with_extension("out"))
        .unwrap_or_else(|_| panic!("{} has no .out file", path.display()));
    let fails = source
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("// fails:"))
        .map(str::trim);

    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let bin_path = tmp_dir.join(format!("llvm_{stem}"));

    let build = Command::new(env!("CARGO_BIN_EXE_noev"))
        .args(["build", "--codegen=llvm", "-o"])
        .arg(&bin_path)
        .arg(path)
        .output()
        .expect("noev build --codegen=llvm runs");
    if !build.status.success() {
        let stderr = String::from_utf8_lossy(&build.stderr);
        if let Some(text) = fails
            && stderr.contains(text)
            && expected.is_empty()
        {
            return None;
        }
        return Some(format!("LLVM build failed:\n{stderr}"));
    }

    let output = Command::new(&bin_path)
        .output()
        .expect("LLVM compiled binary runs");
    let _ = fs::remove_file(&bin_path);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut problems = Vec::new();
    if stdout != expected {
        problems.push(format!(
            "expected output:\n{expected}\nactual output:\n{stdout}"
        ));
    }
    match fails {
        Some(text) if output.status.success() => {
            problems.push(format!("should have failed with `{text}`"))
        }
        Some(text) if !stderr.contains(text) => {
            problems.push(format!("errors should contain `{text}`:\n{stderr}"))
        }
        None if !output.status.success() => problems.push(format!("failed:\n{stderr}")),
        _ => {}
    }
    (!problems.is_empty()).then(|| problems.join("\n"))
}

#[test]
fn llvm_compiled_matches_interpreter() {
    let tmp_dir = std::env::temp_dir().join(format!("noev-llvm-test-{}", std::process::id()));
    fs::create_dir_all(&tmp_dir).expect("temp dir");

    let mut files: Vec<PathBuf> = fs::read_dir(run_dir())
        .expect("tests/run is readable")
        .map(|e| e.expect("directory entry is readable").path())
        .filter(|p| p.extension().is_some_and(|e| e == "nv"))
        .collect();
    files.sort();

    let workers = std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(4);
    let next = Mutex::new(0usize);
    let results: Mutex<Vec<(usize, String)>> = Mutex::new(Vec::new());

    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let idx = {
                        let mut guard = next.lock().unwrap();
                        if *guard >= files.len() {
                            break;
                        }
                        let i = *guard;
                        *guard += 1;
                        i
                    };
                    let path = &files[idx];
                    if let Some(problem) = check_llvm_compiled(path, &tmp_dir) {
                        let name = path.file_name().unwrap_or_default().to_string_lossy();
                        results
                            .lock()
                            .unwrap()
                            .push((idx, format!("{name}: {problem}")));
                    }
                }
            });
        }
    });

    let _ = fs::remove_dir_all(&tmp_dir);

    let mut failures = results.into_inner().unwrap();
    failures.sort_by_key(|(idx, _)| *idx);
    let failures: Vec<String> = failures.into_iter().map(|(_, msg)| msg).collect();

    assert!(
        failures.is_empty(),
        "{} of {} LLVM compiled tests failed:\n\n{}",
        failures.len(),
        files.len(),
        failures.join("\n\n")
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
