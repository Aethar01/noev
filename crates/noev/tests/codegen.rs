//! Compiles every program in `tests/run/` to a native binary with Cranelift
//! (`noev build --emit=binary`) and verifies that its output and failure behaviour match the
//! interpreter's conformance suite.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

fn run_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/run")
}

fn check_compiled(path: &Path, tmp_dir: &Path) -> Option<String> {
    let source = fs::read_to_string(path).expect("the program is readable");
    let expected = fs::read_to_string(path.with_extension("out"))
        .unwrap_or_else(|_| panic!("{} has no .out file", path.display()));
    let fails = source
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("// fails:"))
        .map(str::trim);

    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let bin_path = tmp_dir.join(format!("test_{stem}"));

    let build = Command::new(env!("CARGO_BIN_EXE_noev"))
        .args(["build", "-o"])
        .arg(&bin_path)
        .arg(path)
        .output()
        .expect("noev build runs");
    if !build.status.success() {
        let stderr = String::from_utf8_lossy(&build.stderr);
        if let Some(text) = fails
            && stderr.contains(text)
            && expected.is_empty()
        {
            return None;
        }
        return Some(format!("build failed:\n{stderr}"));
    }

    let output = Command::new(&bin_path)
        .output()
        .expect("compiled binary runs");
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
fn compiled_matches_interpreter() {
    let tmp_dir = std::env::temp_dir().join(format!("noev-codegen-test-{}", std::process::id()));
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
                    if let Some(problem) = check_compiled(path, &tmp_dir) {
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
        "{} of {} compiled tests failed:\n\n{}",
        failures.len(),
        files.len(),
        failures.join("\n\n")
    );
}

#[test]
fn emit_object_produces_native_object() {
    let tmp_dir = std::env::temp_dir().join(format!("noev-obj-test-{}", std::process::id()));
    fs::create_dir_all(&tmp_dir).expect("temp dir");
    let obj_path = tmp_dir.join("out.o");
    let src = fs::read_dir(run_dir())
        .expect("tests/run is readable")
        .map(|e| e.expect("entry").path())
        .find(|p| p.extension().is_some_and(|e| e == "nv") && !p.to_string_lossy().contains("fail"))
        .expect("at least one .nv file");

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
