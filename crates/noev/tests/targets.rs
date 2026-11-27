//! End-to-end tests for Step 17: targets, capability checks, `noev target fetch/list/show`,
//! `[target]` in `noev.toml`, `impl Backend` rules, and C plug-in backends (`spec/runtime.md` §3–5).

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
    let dir = std::env::temp_dir().join(format!("noev_targets_{name}_{}", std::process::id()));
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

/// Compiles a test C plug-in backend against `include/noev_backend.h` that supports BOTH:
/// - `run_circuit`: emits deterministic 50/50 counts for `0` and `(1 << n_clbits) - 1`, and
/// - `session_*`: a lightweight classical Z-basis simulator supporting `X` (and controlled `X`),
///   `Z`, `measure`, `reset`, and `release`.
#[cfg(unix)]
fn build_demo_plugin(dir: &Path) -> PathBuf {
    let c_src = dir.join("demo_plugin.c");
    let so_path = dir.join("libdemo_qpu.so");
    let code = r#"
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "noev_backend.h"

typedef struct {
    char device[64];
} DemoBackend;

typedef struct {
    uint64_t state;
    size_t next_q;
} DemoSession;

static void *demo_create(const char *device_or_null) {
    DemoBackend *b = (DemoBackend *)calloc(1, sizeof(DemoBackend));
    if (!b) return NULL;
    if (device_or_null) {
        snprintf(b->device, sizeof(b->device), "%s", device_or_null);
    }
    return b;
}

static void demo_destroy(void *backend) {
    free(backend);
}

static bool demo_run_circuit(
    void *backend,
    const char *qasm3,
    size_t qasm3_len,
    size_t n_qubits,
    size_t n_clbits,
    size_t shots,
    uint64_t seed,
    NoevCountCallback on_count,
    void *cb_env,
    char *err_buf,
    size_t err_cap
) {
    (void)backend; (void)n_qubits; (void)seed;
    if (!qasm3 || qasm3_len == 0) {
        snprintf(err_buf, err_cap, "empty qasm3");
        return false;
    }
    size_t half = shots / 2;
    uint64_t all_ones = (n_clbits >= 64) ? ~0ULL : ((1ULL << n_clbits) - 1ULL);
    if (half > 0) on_count(cb_env, 0, half);
    if (shots - half > 0) on_count(cb_env, all_ones, shots - half);
    return true;
}

static void *demo_session_new(void *backend, uint64_t seed) {
    (void)backend; (void)seed;
    return calloc(1, sizeof(DemoSession));
}

static void demo_session_free(void *session) {
    free(session);
}

static bool demo_add_qubit(void *session, size_t *out_qubit, char *err_buf, size_t err_cap) {
    DemoSession *s = (DemoSession *)session;
    if (s->next_q >= 64) {
        snprintf(err_buf, err_cap, "too many qubits");
        return false;
    }
    *out_qubit = s->next_q++;
    return true;
}

static bool demo_apply(
    void *session,
    uint8_t gate_id,
    bool adjoint,
    const double *params,
    size_t n_params,
    const size_t *controls,
    size_t n_controls,
    const size_t *targets,
    size_t n_targets,
    char *err_buf,
    size_t err_cap
) {
    (void)adjoint; (void)params; (void)n_params; (void)err_buf; (void)err_cap;
    DemoSession *s = (DemoSession *)session;
    for (size_t i = 0; i < n_controls; i++) {
        if (((s->state >> controls[i]) & 1ULL) == 0ULL) {
            return true;
        }
    }
    if (gate_id == NOEV_GATE_X && n_targets == 1) {
        s->state ^= (1ULL << targets[0]);
    } else if (gate_id == NOEV_GATE_SWAP && n_targets == 2) {
        uint64_t a = (s->state >> targets[0]) & 1ULL;
        uint64_t b = (s->state >> targets[1]) & 1ULL;
        if (a != b) {
            s->state ^= (1ULL << targets[0]) | (1ULL << targets[1]);
        }
    }
    return true;
}

static bool demo_measure(void *session, size_t qubit, bool *out_bit, char *err_buf, size_t err_cap) {
    (void)err_buf; (void)err_cap;
    DemoSession *s = (DemoSession *)session;
    *out_bit = ((s->state >> qubit) & 1ULL) != 0ULL;
    return true;
}

static bool demo_reset(void *session, size_t qubit, char *err_buf, size_t err_cap) {
    (void)err_buf; (void)err_cap;
    DemoSession *s = (DemoSession *)session;
    s->state &= ~(1ULL << qubit);
    return true;
}

static bool demo_release(void *session, size_t qubit, char *err_buf, size_t err_cap) {
    DemoSession *s = (DemoSession *)session;
    if (((s->state >> qubit) & 1ULL) != 0ULL) {
        snprintf(err_buf, err_cap, "released qubit not in |0>");
        return false;
    }
    return true;
}

static const NoevBackendVTable VTABLE = {
    .abi_version = NOEV_BACKEND_ABI_VERSION,
    .name = "demo_qpu",
    .level = NOEV_LEVEL_BOTH,
    .qubits = 8,
    .capabilities = NOEV_CAP_CHECK_RELEASE | NOEV_CAP_FOREIGN_WHILE_LIVE,
    .target_toml = NULL,
    .create = demo_create,
    .destroy = demo_destroy,
    .run_circuit = demo_run_circuit,
    .session_new = demo_session_new,
    .session_free = demo_session_free,
    .session_add_qubit = demo_add_qubit,
    .session_apply = demo_apply,
    .session_measure = demo_measure,
    .session_reset = demo_reset,
    .session_release = demo_release,
};

const NoevBackendVTable *noev_backend_vtable(void) {
    return &VTABLE;
}
"#;
    fs::write(&c_src, code).expect("write demo_plugin.c");
    let include_dir = repo_root().join("include");
    let out = Command::new("cc")
        .args(["-shared", "-fPIC", "-O2", "-Wall", "-Wextra", "-I"])
        .arg(&include_dir)
        .arg(&c_src)
        .arg("-o")
        .arg(&so_path)
        .output()
        .expect("cc builds demo plug-in");
    assert!(
        out.status.success(),
        "cc failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    so_path
}

#[cfg(unix)]
#[test]
fn c_plugin_fetch_run_and_compiled_binary() {
    ensure_runtime_lib();
    let dir = temp_dir("plugin_e2e");
    let cache_dir = dir.join("cache");
    let circuits_dir = dir.join("circuits");
    let plugin_so = build_demo_plugin(&dir);

    // 1. `noev target fetch demo_qpu:chip1 --from <plugin.so>`
    let fetch = Command::new(noev_bin())
        .env("NOEV_TARGET_CACHE", &cache_dir)
        .args(["target", "fetch", "demo_qpu:chip1", "--from"])
        .arg(&plugin_so)
        .output()
        .expect("noev target fetch");
    assert!(
        fetch.status.success(),
        "target fetch failed:\n{}",
        String::from_utf8_lossy(&fetch.stderr)
    );

    // 2. `noev target list` and `noev target show demo_qpu:chip1`
    let list = Command::new(noev_bin())
        .env("NOEV_TARGET_CACHE", &cache_dir)
        .args(["target", "list"])
        .output()
        .expect("noev target list");
    assert!(list.status.success());
    let list_out = String::from_utf8_lossy(&list.stdout);
    assert!(list_out.contains("ketrs"), "list missing ketrs: {list_out}");
    assert!(
        list_out.contains("demo_qpu:chip1"),
        "list missing cached target: {list_out}"
    );

    let show = Command::new(noev_bin())
        .env("NOEV_TARGET_CACHE", &cache_dir)
        .args(["target", "show", "demo_qpu:chip1"])
        .output()
        .expect("noev target show");
    assert!(show.status.success());
    let show_out = String::from_utf8_lossy(&show.stdout);
    assert!(show_out.contains("name = \"demo_qpu\""), "{show_out}");
    assert!(show_out.contains("device = \"chip1\""), "{show_out}");
    assert!(show_out.contains("qubits = 8"), "{show_out}");

    // 3. `noev run --backend=demo_qpu:chip1` running BOTH circuit `sample` AND gate-level ops.
    let prog = dir.join("bell_and_gate.nv");
    fs::write(
        &prog,
        r#"
fn main() {
    let counts = sample 20 {
        let mut a = |0>
        let mut b = |0>
        h(mut a)
        ctrl a { x(mut b) }
        (mv a.measure(), mv b.measure())
    }
    print(counts)

    let mut q0 = |0>
    let mut q1 = |0>
    x(mut q0)
    ctrl q0 { x(mut q1) }
    print("gate={mv q0.measure()}{mv q1.measure()}")
}
"#,
    )
    .expect("write prog");

    let run = Command::new(noev_bin())
        .env("NOEV_TARGET_CACHE", &cache_dir)
        .args(["run", "--backend=demo_qpu:chip1", "--circuits"])
        .arg(&circuits_dir)
        .arg(&prog)
        .output()
        .expect("noev run on plugin");
    assert!(
        run.status.success(),
        "run on plugin failed:\n{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("00  10   50.0%"), "stdout:\n{stdout}");
    assert!(stdout.contains("11  10   50.0%"), "stdout:\n{stdout}");
    assert!(stdout.contains("gate=11"), "stdout:\n{stdout}");
    assert!(circuits_dir.join("circuit-1.qasm").is_file());

    // 4. `noev build --backend=demo_qpu:chip1` + running native binary via `NOEV_BACKEND` and `--backend`
    let bin_path = dir.join("bell_and_gate");
    let build = Command::new(noev_bin())
        .env("NOEV_TARGET_CACHE", &cache_dir)
        .args(["build", "--backend=demo_qpu:chip1", "-o"])
        .arg(&bin_path)
        .arg(&prog)
        .output()
        .expect("noev build");
    assert!(
        build.status.success(),
        "build failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    let bin_env = Command::new(&bin_path)
        .env("NOEV_TARGET_CACHE", &cache_dir)
        .env("NOEV_BACKEND", "demo_qpu:chip1")
        .output()
        .expect("run native binary with NOEV_BACKEND");
    assert!(
        bin_env.status.success(),
        "binary failed:\n{}",
        String::from_utf8_lossy(&bin_env.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&bin_env.stdout), stdout);

    let bin_flag = Command::new(&bin_path)
        .arg("--backend")
        .arg(format!("{}:chip1", plugin_so.display()))
        .output()
        .expect("run native binary with --backend path");
    assert!(
        bin_flag.status.success(),
        "binary --backend failed:\n{}",
        String::from_utf8_lossy(&bin_flag.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&bin_flag.stdout), stdout);
}

#[test]
fn target_capability_checks() {
    let dir = temp_dir("cap_checks");

    // Restricted target: circuit-only, 2 qubits, non-universal gates ["x", "z"], no read_state, no foreign_while_live.
    let target_toml = dir.join("strict.toml");
    fs::write(
        &target_toml,
        r#"
name = "strict_qpu"
level = "circuit"
qubits = 2
gates = ["x", "z"]

[capabilities]
read_state = false
check_release = false
exact_expectations = false
sample_state = false
foreign_while_live = false
"#,
    )
    .expect("write strict.toml");

    // 1. `amplitudes` rejected when `read_state = false`
    let amp_nv = dir.join("amp.nv");
    fs::write(
        &amp_nv,
        r#"
use std::debug::amplitudes
fn main() {
    let q = |0>
    let _ = amplitudes(ref q)
    mv q.discard()
}
"#,
    )
    .unwrap();
    let out = Command::new(noev_bin())
        .args(["check", "--backend"])
        .arg(&target_toml)
        .arg(&amp_nv)
        .output()
        .unwrap();
    assert!(!out.status.success(), "amplitudes should be rejected");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("read_state"), "stderr:\n{err}");

    // 2. Non-circuit `sample` rejected when `level = "circuit"`
    let non_circuit_nv = dir.join("non_circuit.nv");
    fs::write(
        &non_circuit_nv,
        r#"
fn main() {
    let _ = sample 4 {
        let mut q = |0>
        let mut r = |0>
        x(mut q)
        let b = mv q.measure()
        let angle = if b == 1 { 1.0 } else { 0.0 }
        rx(mut r, angle)
        mv r.measure()
    }
}
"#,
    )
    .unwrap();
    let out = Command::new(noev_bin())
        .args(["check", "--backend"])
        .arg(&target_toml)
        .arg(&non_circuit_nv)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "non-circuit sample should be rejected"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("only accepts circuits"), "stderr:\n{err}");

    // 3. Too many qubits rejected when `qubits = 2`
    let too_many_q = dir.join("three_q.nv");
    fs::write(
        &too_many_q,
        r#"
fn main() {
    let _ = sample 4 {
        let q = |000>
        mv q.measure()
    }
}
"#,
    )
    .unwrap();
    let out = Command::new(noev_bin())
        .args(["check", "--backend"])
        .arg(&target_toml)
        .arg(&too_many_q)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "3 qubits should be rejected on 2-qubit target"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("has only 2"), "stderr:\n{err}");

    // 4. Unsupported gate (`h`) rejected on non-universal gate set `["x", "z"]`
    let bad_gate = dir.join("bad_gate.nv");
    fs::write(
        &bad_gate,
        r#"
fn main() {
    let _ = sample 4 {
        let mut q = |0>
        h(mut q)
        mv q.measure()
    }
}
"#,
    )
    .unwrap();
    let out = Command::new(noev_bin())
        .args(["check", "--backend"])
        .arg(&target_toml)
        .arg(&bad_gate)
        .output()
        .unwrap();
    assert!(!out.status.success(), "h gate should be rejected");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("gate `h` is not supported"), "stderr:\n{err}");

    // 5. Foreign call while qubits are alive rejected when `foreign_while_live = false`
    let foreign_live = dir.join("foreign_live.nv");
    fs::write(
        &foreign_live,
        r#"
extern fn fabs(x: Float) -> Float
fn main() {
    let q = |0>
    let _ = unsafe { fabs(-1.0) }
    mv q.discard()
}
"#,
    )
    .unwrap();
    let out = Command::new(noev_bin())
        .args(["check", "--backend"])
        .arg(&target_toml)
        .arg(&foreign_live)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "foreign call with live qubit should be rejected"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("while qubits are alive"), "stderr:\n{err}");

    // 6. Valid circuit within target capabilities passes!
    let valid_nv = dir.join("valid.nv");
    fs::write(
        &valid_nv,
        r#"
fn main() {
    let _ = sample 4 {
        let mut a = |0>
        let mut b = |0>
        x(mut a)
        z(mut b)
        (mv a.measure(), mv b.measure())
    }
}
"#,
    )
    .unwrap();
    let out = Command::new(noev_bin())
        .args(["check", "--backend"])
        .arg(&target_toml)
        .arg(&valid_nv)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "valid program should pass:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn backend_impl_cannot_touch_qubits() {
    let dir = temp_dir("backend_no_qubits");
    let bad = dir.join("bad_backend.nv");
    fs::write(
        &bad,
        r#"
use std::backend::{Backend, BackendError, Capabilities, Circuit, Level, ShotCount, Shots, Target}

struct Bad {
    id: Int
}

impl Backend for Bad {
    fn target(ref self) -> Target {
        Target {
            name: "bad",
            level: Level::Circuit,
            qubits: 1,
            gates: Vec::<String>::new(),
            coupling: Vec::<(Int, Int)>::new(),
            capabilities: Capabilities {
                read_state: false,
                check_release: false,
                exact_expectations: false,
                sample_state: false,
                foreign_while_live: false,
            },
        }
    }

    fn run(mut self, _circuit: ref Circuit, shots: Int) -> Result<Shots, BackendError> {
        let q = |0>
        mv q.discard()
        let mut counts = Vec::<ShotCount>::new()
        counts.push(ShotCount { bits: 0, count: shots })
        Ok(Shots { counts: mv counts })
    }
}

fn main() {}
"#,
    )
    .unwrap();

    let out = Command::new(noev_bin())
        .args(["check"])
        .arg(&bad)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "Backend impl creating qubits must fail"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("implementing `std::backend::Backend` cannot create or act on qubits"),
        "stderr:\n{err}"
    );
}

#[test]
fn manifest_target_section_is_honored() {
    let dir = temp_dir("manifest_target");
    let cache_dir = dir.join("cache");
    let pkg_dir = dir.join("pkg");
    fs::create_dir_all(pkg_dir.join("src")).unwrap();

    // Cache a 1-qubit target named `tiny`.
    let tiny_toml = dir.join("tiny.toml");
    fs::write(
        &tiny_toml,
        r#"
name = "tiny"
level = "circuit"
qubits = 1

[capabilities]
read_state = false
check_release = false
exact_expectations = false
sample_state = false
foreign_while_live = true
"#,
    )
    .unwrap();
    let fetch = Command::new(noev_bin())
        .env("NOEV_TARGET_CACHE", &cache_dir)
        .args(["target", "fetch", "tiny", "--from"])
        .arg(&tiny_toml)
        .output()
        .unwrap();
    assert!(fetch.status.success());

    fs::write(
        pkg_dir.join("noev.toml"),
        r#"
[package]
name = "tiny_app"

[target]
backend = "tiny"
"#,
    )
    .unwrap();
    fs::write(
        pkg_dir.join("src/main.nv"),
        r#"
fn main() {
    let _ = sample 4 {
        let q = |00>
        mv q.measure()
    }
}
"#,
    )
    .unwrap();

    // Default `noev check` picks up `[target] backend = "tiny"` and rejects 2 qubits.
    let fail = Command::new(noev_bin())
        .env("NOEV_TARGET_CACHE", &cache_dir)
        .args(["check"])
        .arg(&pkg_dir)
        .output()
        .unwrap();
    assert!(
        !fail.status.success(),
        "should fail against manifest target `tiny`"
    );
    assert!(
        String::from_utf8_lossy(&fail.stderr).contains("has only 1"),
        "{}",
        String::from_utf8_lossy(&fail.stderr)
    );

    // Overriding with `--backend=ketrs` succeeds!
    let pass = Command::new(noev_bin())
        .env("NOEV_TARGET_CACHE", &cache_dir)
        .args(["check", "--backend=ketrs"])
        .arg(&pkg_dir)
        .output()
        .unwrap();
    assert!(
        pass.status.success(),
        "--backend=ketrs override failed:\n{}",
        String::from_utf8_lossy(&pass.stderr)
    );
}
