//! Runs the conformance suite in `tests/ui` against the `noev` binary.
//!
//! The suite's format is described in `tests/ui/README.md`. Rejected programs mark each
//! expected error with a `//~ N001` comment on its line; programs under `tests/ui/pass` must
//! check with no errors.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Error counts keyed by (line, code).
type Errors = BTreeMap<(usize, String), usize>;

fn ui_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/ui")
}

fn nv_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("tests/ui is readable") {
        let path = entry.expect("directory entry is readable").path();
        if path.is_dir() {
            nv_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "nv") {
            out.push(path);
        }
    }
}

fn expected_errors(source: &str) -> Errors {
    let mut errors = Errors::new();
    for (index, line) in source.lines().enumerate() {
        if let Some((_, codes)) = line.split_once("//~") {
            for code in codes.split_whitespace() {
                *errors.entry((index + 1, code.to_string())).or_default() += 1;
            }
        }
    }
    errors
}

/// Parses `FILE:LINE:COLUMN: error[CODE]: message` lines.
fn reported_errors(output: &str) -> Errors {
    let mut errors = Errors::new();
    for line in output.lines() {
        let Some((location, rest)) = line.split_once(": error[") else {
            continue;
        };
        let Some((code, _)) = rest.split_once(']') else {
            continue;
        };
        let mut parts = location.rsplitn(3, ':');
        let (Some(_column), Some(line_number)) = (parts.next(), parts.next()) else {
            continue;
        };
        if let Ok(line_number) = line_number.parse() {
            *errors.entry((line_number, code.to_string())).or_default() += 1;
        }
    }
    errors
}

fn describe(errors: &Errors) -> String {
    errors
        .iter()
        .map(|((line, code), count)| match count {
            1 => format!("{code} on line {line}"),
            n => format!("{n} × {code} on line {line}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Checks one file, returning a description of what went wrong, if anything.
fn check_file(path: &Path) -> Option<String> {
    let source = fs::read_to_string(path).expect("test file is readable");
    let expected = expected_errors(&source);

    let output = Command::new(env!("CARGO_BIN_EXE_noev"))
        .args(["check", "--message-format=short"])
        .arg(path)
        .output()
        .expect("noev runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let reported = reported_errors(&stderr);

    let expected_status = if expected.is_empty() { 0 } else { 1 };
    match output.status.code() {
        Some(status) if status == expected_status => {}
        Some(2) | None => return Some(format!("noev could not check it:\n{stderr}")),
        Some(status) => {
            return Some(format!("exit status {status}, expected {expected_status}"));
        }
    }

    let mut missing = Errors::new();
    let mut unexpected = Errors::new();
    for (key, &want) in &expected {
        let got = reported.get(key).copied().unwrap_or(0);
        if got < want {
            missing.insert(key.clone(), want - got);
        }
    }
    for (key, &got) in &reported {
        let want = expected.get(key).copied().unwrap_or(0);
        if got > want {
            unexpected.insert(key.clone(), got - want);
        }
    }
    if missing.is_empty() && unexpected.is_empty() {
        return None;
    }
    let mut problems = Vec::new();
    if !missing.is_empty() {
        problems.push(format!("missing: {}", describe(&missing)));
    }
    if !unexpected.is_empty() {
        problems.push(format!("unexpected: {}", describe(&unexpected)));
    }
    Some(problems.join("; "))
}

#[test]
fn ui() {
    let root = ui_dir();
    let mut files = Vec::new();
    nv_files(&root, &mut files);
    files.sort();
    assert!(!files.is_empty(), "no .nv files under {}", root.display());

    let failures: Vec<String> = files
        .iter()
        .filter_map(|path| {
            let name = path.strip_prefix(&root).unwrap_or(path).display();
            check_file(path).map(|problem| format!("{name}: {problem}"))
        })
        .collect();

    assert!(
        failures.is_empty(),
        "{} of {} UI tests failed:\n\n{}",
        failures.len(),
        files.len(),
        failures.join("\n\n")
    );
}
