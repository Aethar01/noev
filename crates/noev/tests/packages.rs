//! Runs the packages in `tests/packages` against the `noev` binary, as `tests/packages/README.md`
//! describes.

use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn packages() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/packages");
    let mut names: Vec<String> = fs::read_dir(&dir)
        .expect("tests/packages is readable")
        .map(|e| e.expect("directory entry is readable").path())
        .filter(|p| p.is_dir())
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut failures = Vec::new();
    let mut checked = 0;
    for name in &names {
        let package = dir.join(name);
        if let Ok(expected) = fs::read_to_string(package.join("expected.stdout")) {
            checked += 1;
            let output = Command::new(env!("CARGO_BIN_EXE_noev"))
                .args(["run", name, "--seed", "1"])
                .current_dir(&dir)
                .output()
                .expect("noev runs");
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !output.status.success() || stdout != expected {
                failures.push(format!(
                    "{name}: expected to print\n{expected}\nbut printed\n{stdout}\nand \
                     reported\n{}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }
        if let Ok(expected) = fs::read_to_string(package.join("expected.stderr")) {
            checked += 1;
            let output = Command::new(env!("CARGO_BIN_EXE_noev"))
                .args(["check", name, "--message-format=short"])
                .current_dir(&dir)
                .output()
                .expect("noev runs");
            let stderr = String::from_utf8_lossy(&output.stderr);
            let should_fail = expected.contains(": error");
            if stderr != expected || output.status.success() == should_fail {
                failures.push(format!(
                    "{name}: expected\n{expected}\nbut reported\n{stderr}(exit status {})",
                    output.status
                ));
            }
        }
    }
    assert!(
        checked >= 5,
        "found only {checked} packages with expected output"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
