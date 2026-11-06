//! Runs the conformance suite in `tests/run` against the `noev` binary.
//!
//! The suite's format is described in `tests/run/README.md`: each `NAME.nv` must print exactly
//! `NAME.out`, and one starting with `// fails: TEXT` must fail with `TEXT` in its errors.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/run")
}

/// What is wrong with one program's run, if anything.
fn check(path: &Path) -> Option<String> {
    let source = fs::read_to_string(path).expect("the program is readable");
    let expected = fs::read_to_string(path.with_extension("out"))
        .unwrap_or_else(|_| panic!("{} has no .out file", path.display()));
    let fails = source
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("// fails:"))
        .map(str::trim);
    let output = Command::new(env!("CARGO_BIN_EXE_noev"))
        .arg("run")
        .arg(path)
        .output()
        .expect("noev runs");
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
fn run() {
    let mut files: Vec<PathBuf> = fs::read_dir(run_dir())
        .expect("tests/run is readable")
        .map(|e| e.expect("directory entry is readable").path())
        .filter(|p| p.extension().is_some_and(|e| e == "nv"))
        .collect();
    files.sort();
    let failures: Vec<String> = files
        .iter()
        .filter_map(|path| {
            check(path).map(|problem| {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                format!("{name}: {problem}")
            })
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} run tests failed:\n\n{}",
        failures.len(),
        files.len(),
        failures.join("\n\n")
    );
}
